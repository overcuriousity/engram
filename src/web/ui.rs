use crate::core::search::SearchQuery;
use crate::error::Result;
use crate::fmt::{ago, fmt_time};
use crate::store::corpora::CorpusStatus;
use crate::tenants::Tenant;
use crate::web::auth_routes::HtmlTemplate;
use crate::web::markdown;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Query};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};

#[derive(Clone)]
pub struct RenderedResult {
    pub artifact_id: String,
    /// Empty where the artifact has no title; the rail then renders no heading (see `render_hit`).
    pub title: String,
    /// The section or note this text sits in, as a qualifier under the snippet; empty where it would repeat it.
    pub section: String,
    /// Sanitized HTML from `markdown::render`. Rendered with `|safe`.
    pub html: String,
    pub snippet: String,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub corpus_id: String,
    /// `#1`, `#2`, …, not the raw score, which is incomparable between lists. Empty for a weak result.
    pub rank: String,
    /// See `SearchResult::weak`.
    pub weak: bool,
    /// Moved up on activation: it passed a near-tie, not a better match.
    pub primed: bool,
    /// Kept apart from `primed`: "just reading this" and "reached often" are different reasons.
    pub in_sitting: bool,
    /// "in 2 h" when a reminder on this artifact is due inside the horizon.
    pub due_in: Option<String>,
    /// Past the relevance cliff: greyed, rank kept. See `search::cliff`.
    pub past_cliff: bool,
    /// Badged, so a sunk row does not read as a ranking bug.
    pub retired: bool,
    /// Set only on an associated hit.
    pub via_title: Option<String>,
    /// Badged, so model text is never indistinguishable from captured text.
    pub model_written: bool,
    /// How many corpora it draws from, for the badge.
    pub origin_count: usize,
    /// The judge's line, where the link was judged.
    pub reason: Option<String>,
    /// Only where the request asked; see `why_ranked`.
    pub why_ranked: Option<String>,
    /// The next passage did not place in this list; false where it did (`continues_in` names it).
    pub continues: bool,
    /// The next passage's rank where it is in this list; empty otherwise.
    pub continues_in: String,
    /// The rail names both sides and picks neither.
    pub disagrees: Vec<DisagreementRow>,
}

/// One line under a result: `Disagrees with <title> (day): <what differs>`.
#[derive(Clone)]
pub struct DisagreementRow {
    pub other_id: String,
    pub other_title: String,
    /// `%Y-%m-%d`, the short day the idle page's recent rows use.
    pub day: String,
    pub detail: Option<String>,
}

#[derive(Default)]
pub struct QueueRow {
    pub id: String,
    pub label: String,
    /// Never rendered alone: tells apart two rows synthesis gave one name. Empty for a photo or unextracted PDF.
    pub opening: String,
    pub status: String,
    pub badge: &'static str,
    pub artifact_count: i64,
    pub created: String,
    /// `3/9` while windows are being segmented, `None` once all resolved.
    pub progress: Option<String>,
    /// Already formatted; `—` before the capture is read.
    pub coverage: String,
    pub low_coverage: bool,
    /// False for a capture with no segment rows, whose loss cannot be placed; the `#uncovered` link is dropped.
    pub locatable: bool,
    /// The open questions this capture answered, in the operator's words; silent when empty.
    pub covered: Vec<String>,
    /// Only these announce themselves.
    pub in_flight: bool,
    /// False for failed, parked and partial, where a count of artifacts misleads.
    pub settled: bool,
    /// About to get a title, so shown differently from one that has none.
    pub unnamed: bool,
}

pub struct ArtifactView {
    pub id: String,
    pub title: String,
    /// Sanitized by `markdown::render`. One of the few `|safe` interpolations.
    pub html: String,
    pub text: String,
    pub tags: Vec<String>,
    pub embed_state: String,
    pub embed_badge: &'static str,
}

pub fn status_badge(status: &crate::store::corpora::CorpusStatus) -> &'static str {
    use crate::store::corpora::CorpusStatus::*;
    match status {
        Ready => "success",
        // NeedsReview is only an older build's parked capture.
        Partial | NeedsReview => "warning",
        Failed => "danger",
        Describing | Extracting | Raw | Segmenting | Segmented | Embedding => "none",
    }
}

pub fn embed_badge(state: &crate::store::artifacts::EmbedState) -> &'static str {
    use crate::store::artifacts::EmbedState::*;
    match state {
        Embedded => "success",
        Failed => "danger",
        Pending => "none",
    }
}

/// The kit's status symbol: coloured icon, then the text in normal colour.
pub fn status(kind: &str, text: &str) -> askama::filters::Safe<String> {
    use crate::web::icons::Icon;
    let icon = match kind {
        "success" => Icon::Success.svg(14),
        "warning" => Icon::Warning.svg(14),
        "danger" => Icon::Danger.svg(14),
        "due" => Icon::Due.svg(14),
        _ => String::new(),
    };
    let text = askama::filters::escape(text, askama::filters::Html)
        .map(|e| e.to_string())
        .unwrap_or_default();
    askama::filters::Safe(format!(
        "<span class=\"oc-status oc-status--{kind}\">{icon}<span>{text}</span></span>"
    ))
}

/// Wording for queue identifiers; an unknown stage returns unchanged, never blank.
pub(crate) fn sweep_label(stage: &str) -> &str {
    match stage {
        "synthesize" => "Writing artifacts",
        // No stage constructs this; an older binary's queue can still hold one.
        "enrich" => "Enriching",
        "segment_window" => "Segmenting",
        "title" => "Naming captures",
        "embed" => "Embedding",
        "consolidate" => "Consolidating",
        "dedupe" => "Judging duplicates",
        "relate" => "Finding near-identicals",
        "describe" => "Describing images",
        "extract" => "Reading documents",
        "associate" => "Associating",
        "link_judge" => "Judging links",
        "pursuit" => "Following up questions",
        "generate" => "Answering gaps",
        "retention" => "Retention",
        "arm_dedupe" => "Arming dedupe",
        "context" => "Learning situations",
        "moments" => "Reading time",
        "remind" => "Pushing what is due",
        "reap" => "Reaping the retired",
        "probe" => "Minting probes",
        "condense" => "Condensing",
        "fetch" => "Fetching links again",
        other => other,
    }
}

/// The opening of the body, not the ordinal; `title_of` also strips markup off stored titles.
pub(crate) fn artifact_title(c: &crate::store::artifacts::Chunk) -> String {
    title_of(c)
}

/// A passage is rendered verbatim, since markdown would eat `#` and joined lines; everything else is model-written markdown.
pub(crate) fn artifact_html(c: &crate::store::artifacts::Chunk) -> String {
    if c.provenance == crate::store::artifacts::Provenance::Passage {
        markdown::render_verbatim(&c.text)
    } else {
        markdown::render(&c.text)
    }
}

pub(crate) fn artifact_view(c: &crate::store::artifacts::Chunk) -> ArtifactView {
    ArtifactView {
        id: c.id.clone(),
        // Empty for a passage and a note, whose first line is their body.
        title: artifact_title(c),
        html: artifact_html(c),
        text: c.text.clone(),
        tags: c.tags.clone(),
        embed_state: c.embed_state.as_str().to_string(),
        embed_badge: embed_badge(&c.embed_state),
    }
}

#[derive(Template)]
#[template(path = "_intent_echo.html")]
pub(crate) struct IntentEchoTemplate {
    /// `will be synthesized`, `large paste`, or empty for an empty box.
    pub(crate) kind: &'static str,
    pub(crate) detail: String,
}

/// An echo that cannot render is no echo, never a 500.
pub(crate) fn render_echo(t: &IntentEchoTemplate) -> String {
    t.render().unwrap_or_default()
}

/// Same counter and budget as the size fork, no model call.
/// `lang` because the ten prompts leave different budgets.
pub(crate) fn fate_echo(
    core: &crate::core::Core,
    q: &str,
    lang: crate::infer::lang::Lang,
) -> IntentEchoTemplate {
    if q.trim().is_empty() {
        return IntentEchoTemplate {
            kind: "",
            detail: String::new(),
        };
    }
    // The embed-only floor: there is no fork, every capture is passages.
    if !core.synthesizes() {
        return IntentEchoTemplate {
            kind: "stored as written",
            detail: "no synthesizer is configured on this base; what you capture is kept \
                     verbatim and found by meaning"
                .into(),
        };
    }
    // No budget means this base stores the text and rewrites nothing.
    let Some(budget) = crate::jobs::synthesize::segment_budget(core, lang) else {
        return IntentEchoTemplate {
            kind: "stored as written",
            detail: "the synthesizer's context is too small to rewrite anything on this                      base; what you capture is kept verbatim"
                .into(),
        };
    };
    let budget = budget.max(1);
    let tokens = core.counter.count(q);
    if tokens <= budget {
        IntentEchoTemplate {
            kind: "will be synthesized",
            detail: "captured verbatim, then rewritten into structured artifacts".into(),
        }
    } else if q.len() <= EXACT_SPLIT_BYTES {
        // The splitter's own answer: it will not cut inside a paragraph, so `div_ceil` under-reports.
        let windows = crate::infer::split::split_into_segments(q, &core.counter, budget).len();
        IntentEchoTemplate {
            kind: "large paste",
            detail: format!("stored verbatim in {windows} windows; synthesis comes with use"),
        }
    } else {
        // Past the bound, a floor estimate: a full split per keystroke is too slow for this route.
        let windows = tokens.div_ceil(budget);
        IntentEchoTemplate {
            kind: "large paste",
            detail: format!(
                "stored verbatim in at least {windows} windows; synthesis comes with use"
            ),
        }
    }
}

/// Above this the window count is estimated; see [`fate_echo`].
const EXACT_SPLIT_BYTES: usize = 20_000;

#[derive(Template)]
#[template(path = "_results.html")]
struct ResultsTemplate {
    results: Vec<RenderedResult>,
    /// Recalled by association, never ranked; shown below the ranked list.
    associated: Vec<RenderedResult>,
    /// Said once above the list instead of on each card.
    all_weak: bool,
    /// The query's indexable terms, for client-side highlighting.
    terms: String,
    /// The refining pass; the tick makes the reordering read as refinement.
    reranked: bool,
    /// Carried by every row so opens, verdicts and gaps name one search; `None` while not recording.
    event_id: Option<String>,
    /// Pre-rendered and shipped out of band; see `intent_echo`.
    echo: String,
    /// Carried by the gap button alone: a later keystroke can fold into the same row. See `Store::gap_event`.
    q: String,
    /// A capture still being read may hold the answer, so no gap is offered.
    reading: bool,
    /// See `gap`.
    held: bool,
}

/// Test-only: a row has twenty fields and a test usually cares about two.
#[cfg(test)]
impl Default for RenderedResult {
    fn default() -> Self {
        Self {
            artifact_id: String::new(),
            title: String::new(),
            section: String::new(),
            html: String::new(),
            snippet: String::new(),
            category: None,
            tags: vec![],
            corpus_id: String::new(),
            rank: String::new(),
            weak: false,
            primed: false,
            in_sitting: false,
            due_in: None,
            past_cliff: false,
            retired: false,
            via_title: None,
            reason: None,
            why_ranked: None,
            model_written: false,
            origin_count: 0,
            continues: false,
            continues_in: String::new(),
            disagrees: Vec::new(),
        }
    }
}

/// Test-only, so a field added to the template breaks every place that renders it.
#[cfg(test)]
impl Default for ResultsTemplate {
    fn default() -> Self {
        Self {
            results: vec![],
            associated: vec![],
            all_weak: false,
            terms: String::new(),
            reranked: false,
            event_id: None,
            echo: String::new(),
            q: String::new(),
            reading: false,
            held: true,
        }
    }
}

impl ResultsTemplate {
    /// Computed, so it cannot disagree with the rows.
    fn loose(&self) -> usize {
        self.results.iter().filter(|r| r.weak).count()
    }

    /// Withheld while a capture is being read and on an empty base, where it says nothing.
    fn gap(&self) -> Option<&str> {
        (!self.reading && self.held)
            .then_some(self.event_id.as_deref())
            .flatten()
    }
}

/// Rendered by the workspace page and by the results endpoint on an emptied box, so idle is one account.
#[derive(Template)]
#[template(path = "_idle_foot.html")]
pub(crate) struct IdleFootTemplate {
    pub(crate) artifacts: i64,
    pub(crate) corpora: i64,
    pub(crate) recent: Vec<IdleRecentRow>,
    /// With nothing held, the line says what the program is for.
    pub(crate) held: bool,
    /// Only on a swap: on first paint the slot already exists in `_box_hint.html`, and a second `id="intent-echo"` would confuse htmx.
    pub(crate) echo: String,
    /// Here rather than in the page, which is built once and would go stale; see `workspace::examples`.
    pub(crate) example_remind: &'static str,
    pub(crate) example_lang: &'static str,
    /// The line says so and polls itself until it is not.
    pub(crate) reading: bool,
    /// The out-of-band clears ride only on a swap; the line's own poll must not empty the heading over results.
    pub(crate) oob: bool,
}

pub(crate) struct IdleRecentRow {
    pub(crate) id: String,
    pub(crate) label: String,
    /// A jog, not a timestamp; see `ago`.
    pub(crate) when: String,
    /// In UTC, the no-JS fallback only: app.js rewrites the href from `at` in the browser's zone.
    pub(crate) day: String,
    /// Unix seconds, for that rewrite.
    pub(crate) at: i64,
}

/// The hint, its opening words, or a word for origins with none; one rule so queue and rail agree.
pub(crate) fn corpus_label(title_hint: Option<String>, raw_text: &str, origin: &str) -> String {
    title_hint.unwrap_or_else(|| {
        if raw_text.is_empty() && origin == crate::core::ingest::ORIGIN_IMAGE {
            "photo".into()
        } else if raw_text.is_empty() && origin == crate::core::ingest::ORIGIN_PDF {
            // A PDF has no opening words until extraction lands; without this the anchor is empty.
            "document".into()
        } else {
            markdown::snippet(raw_text, 60)
        }
    })
}

/// A person's first few captures are when what a paste becomes is not yet known.
pub(crate) const TEACH_UNTIL_SOURCES: i64 = 5;

impl IdleFootTemplate {
    /// Derived, not carried, so the fifth capture takes the list away without a reload.
    fn teach(&self) -> bool {
        self.corpora < TEACH_UNTIL_SOURCES
    }
}

/// Best-effort; a store that cannot say answers no. Written once so the idle line, gap offer and due band agree.
pub(crate) async fn reading_a_capture(store: &crate::store::Store) -> bool {
    store.capture_being_read().await.unwrap_or(false)
}

/// The slimmest reads, run together: this is on a three-second poll per tab while a capture is read.
/// `oob` says whether this is the swap back to idle or the inline first paint.
pub(crate) async fn idle_foot(
    tenant: &Tenant,
    oob: bool,
    headers: &axum::http::HeaderMap,
) -> Result<IdleFootTemplate> {
    let (brief, recent, in_flight) = tokio::join!(
        tenant.core.store.held_brief(),
        tenant.core.store.recent_captures(5),
        reading_a_capture(&tenant.core.store),
    );
    let (corpora, artifacts) = brief?;
    let recent = recent?
        .into_iter()
        .map(
            |(id, title_hint, origin, created_at, opening)| IdleRecentRow {
                day: chrono::DateTime::from_timestamp(created_at, 0)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_default(),
                at: created_at,
                when: ago(created_at),
                label: corpus_label(title_hint, &opening, &origin),
                id,
            },
        )
        .collect();
    let (example_remind, example_lang) = crate::web::workspace::examples(&tenant.core, headers);
    // `ingest_capture` writes the corpus row before the first job, so this agrees with the probe.
    let held = corpora > 0;
    Ok(IdleFootTemplate {
        artifacts,
        corpora,
        recent,
        held,
        example_remind,
        example_lang,
        reading: held && in_flight,
        oob,
        echo: if oob {
            render_echo(&IntentEchoTemplate {
                kind: "",
                detail: String::new(),
            })
        } else {
            String::new()
        },
    })
}

#[derive(Template)]
#[template(path = "_queue.html")]
struct QueueTemplate {
    rows: Vec<QueueRow>,
    /// The fragment polls only while this holds, so an idle page makes no requests.
    active: bool,
}

/// Keyed by stage and field: two sweeps both say `armed`. A field with no entry is not rendered.
const SWEEP_WORDS: &[(&str, &str, &str)] = &[
    ("associate", "events", "searches replayed"),
    ("associate", "verdicts", "verdicts replayed"),
    ("associate", "forgotten", "links forgotten"),
    ("associate", "armed", "links sent to the judge"),
    ("consolidate", "superseded", "artifacts merged"),
    ("consolidate", "judged", "pairs sent to the judge"),
    ("arm_dedupe", "armed", "duplicates sent to the judge"),
    ("retention", "expired", "records expired"),
    ("retention", "named", "gaps named"),
    ("pursuit", "pursuits", "pursuits opened"),
];

pub(crate) fn tally_sweep(stage: &str, detail: &str, totals: &mut Vec<(String, i64)>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(detail) else {
        return;
    };
    for (s, field, word) in SWEEP_WORDS {
        if *s != stage {
            continue;
        }
        let n = v
            .get(field)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if n == 0 {
            continue;
        }
        match totals.iter_mut().find(|(w, _)| w == word) {
            Some((_, t)) => *t += n,
            None => totals.push((word.to_string(), n)),
        }
    }
}

pub struct SourceRow {
    pub id: String,
    pub title: String,
    /// See `RowLabel::named`.
    pub named: bool,
    /// See `SupersededRow::subtitle`.
    pub subtitle: String,
    /// Empty when the source belongs to no corpus, which in practice does not happen.
    pub corpus_id: String,
}

/// Where the row's whole content is this one string: a table cell, a lineage node, a day's list.
pub struct RowLabel {
    pub text: String,
    /// Whether `text` is a given name or the text's opening; the latter is never set as a name.
    pub named: bool,
}

/// A given name, else the text's opening: an empty label would be an invisible link.
pub(crate) fn row_label(c: &crate::store::artifacts::Chunk) -> RowLabel {
    match title_of(c) {
        name if name.is_empty() => RowLabel {
            text: opening_of(c),
            named: false,
        },
        name => RowLabel {
            text: name,
            named: true,
        },
    }
}

pub(crate) fn opening_of(c: &crate::store::artifacts::Chunk) -> String {
    markdown::snippet(&c.text, 60)
}

/// Only where there is a name: otherwise the label is already the opening.
pub(crate) fn row_subtitle(c: &crate::store::artifacts::Chunk) -> String {
    let when = fmt_time(c.created_at);
    match row_label(c).named {
        true => format!("{when} · {}", opening_of(c)),
        false => when,
    }
}

/// Shared by Ops and the detail pane so they agree on what a merge was made of.
pub(crate) async fn source_rows(
    store: &crate::store::Store,
    merged_id: &str,
    roots: &[String],
) -> Vec<SourceRow> {
    let mut sources = Vec::new();
    for rid in roots {
        // A deleted source leaves no row (the `orphaned` flag says so); the self guard covers older bases.
        if rid == merged_id {
            continue;
        }
        if let Ok(r) = store.get_artifact(rid).await {
            let label = row_label(&r);
            sources.push(SourceRow {
                corpus_id: r.corpus_id.clone().unwrap_or_default(),
                title: label.text,
                named: label.named,
                subtitle: row_subtitle(&r),
                id: r.id,
            });
        }
    }
    sources
}

/// Long enough for a real vocabulary, short enough that the row stays a row.
pub(crate) const FACET_LIMIT: usize = 12;

/// Every decision is made here, so the template holds no logic.
#[derive(Default)]
pub struct OfferView {
    pub id: String,
    pub title: String,
    /// Says what the thing is without claiming why, which the random rung must not do.
    pub snippet: String,
    /// Count words on a thin rung ("Twice before"); empty on the random card, which claims nothing.
    pub rung: String,
    /// Empty on the lower two rungs.
    pub blocks: String,
    /// `08.08., 15:04`, or empty.
    pub when: String,
    /// `?rec=<slot>&rung=<rung>`, or empty: tells `artifact_detail` this open came from an offer.
    pub rec: String,
    /// For the `<details>`.
    pub detail: String,
    /// `pattern`, `similar`, `tentative`, `random`; posted back on confirmation and read by `Rung::parse`.
    pub kind: String,
    /// Empty on the random card, which has no cluster.
    pub slot: String,
}

#[derive(Template, Default)]
#[template(path = "_context.html")]
struct ContextTemplate {
    offer: Option<OfferView>,
}

#[derive(serde::Deserialize)]
struct ContextForm {
    #[serde(default)]
    bundle: String,
}

/// Shared by the fragment and `POST /api/v1/context`; records the situation even when nothing is offered.
pub(crate) async fn compute_offer(
    tenant: &Tenant,
    raw_bundle: &str,
) -> Option<(crate::core::recommend::Offer, String)> {
    if !tenant.core.recommends() {
        return None;
    }
    let bundle = crate::core::context::parse_bundle(raw_bundle);
    tenant
        .core
        .record_context_event(raw_bundle, &bundle, Some(&tenant.user.subject));

    // A recommendation that cannot be computed is not worth a 500.
    let o = tenant
        .core
        .offer(Some(&tenant.user.subject), &bundle)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not build a recommendation");
            None
        });

    // Nothing is recorded as shown here: only the browser knows whether the card reached the screen (`/ui/context/seen`).
    // A card with neither a title nor a snippet is not offered, so an unreadable card never counts as shown.
    let o = o?;
    let snippet = match tenant.core.store.get_artifact(&o.artifact_id).await {
        Ok(c) => markdown::snippet(&c.text, 160),
        Err(_) => String::new(),
    };
    (!o.title.is_empty() || !snippet.is_empty()).then_some((o, snippet))
}

async fn context_offer(tenant: Tenant, Form(f): Form<ContextForm>) -> UiResult<Response> {
    let offer = compute_offer(&tenant, &f.bundle)
        .await
        .map(|(o, snippet)| offer_view(o, snippet));
    Ok(HtmlTemplate(ContextTemplate { offer }).into_response())
}

#[derive(serde::Deserialize)]
struct SeenForm {
    artifact_id: String,
    rung: String,
    slot: Option<i64>,
}

/// Posted by `app.js` once the card is on screen; the only writer of `recommended_shown`.
/// Nothing in the form is trusted: the rung goes through `Rung::parse` and the artifact must exist.
async fn context_seen(tenant: Tenant, Form(f): Form<SeenForm>) -> UiResult<Response> {
    record_seen(&tenant, &f.artifact_id, &f.rung, f.slot).await;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

/// Silent on every failure: nothing is waiting on the answer.
pub(crate) async fn record_seen(tenant: &Tenant, artifact_id: &str, rung: &str, slot: Option<i64>) {
    use crate::core::recommend::Rung;
    let Some(rung) = Rung::parse(rung) else {
        return;
    };
    if tenant.core.store.get_artifact(artifact_id).await.is_err() {
        return;
    }
    tenant.core.record_recommendation(
        artifact_id,
        "recommended_shown",
        rung.as_str(),
        slot,
        Some(&tenant.user.subject),
    );
}

fn offer_view(o: crate::core::recommend::Offer, snippet: String) -> OfferView {
    use crate::core::recommend::Rung;
    OfferView {
        // A sentence, not a rung name; the signals are in Details.
        rung: match o.rung {
            Rung::Pattern => {
                "Offered because you tend to open things like this around now — like".to_string()
            }
            Rung::Similar => "Offered because it is like what you opened".to_string(),
            // The undecayed count in words: nobody reads a weight of 1.9 as twice.
            Rung::Tentative => match o.events {
                0 | 1 => "Offered on one earlier occasion like this —".to_string(),
                2 => "Offered on two earlier occasions like this —".to_string(),
                n => format!("Offered on {n} earlier occasions like this —"),
            },
            // Nothing about the situation produced it, so nothing is claimed.
            Rung::Random => String::new(),
        },
        blocks: o.blocks.join(", "),
        // In the zone it happened in.
        when: o
            .at
            .map(|at| {
                let t = crate::core::context::local_time(at, o.at_tz.as_deref(), None);
                format!(
                    "{:02}.{:02}., {:02}:{:02}",
                    t.day,
                    t.month,
                    t.hour as u32,
                    ((t.hour % 1.0) * 60.0).round() as u32
                )
            })
            .unwrap_or_default(),
        // The rung rides on the link, the only place it still exists; every rung, the random one included.
        rec: match o.slot {
            Some(s) => format!("?rec={s}&rung={}", o.rung.as_str()),
            None => format!("?rung={}", o.rung.as_str()),
        },
        // Posted back to confirm the offer was shown, matching what the link carries on a click.
        kind: o.rung.as_str().to_string(),
        slot: o.slot.map(|s| s.to_string()).unwrap_or_default(),
        id: o.artifact_id,
        title: o.title,
        snippet,
        detail: o.detail,
    }
}

/// `count` is 0: missing and crowded out are indistinguishable here, so no number is rendered.
pub(crate) fn ensure_facet(row: &mut Vec<crate::vector::FacetCount>, value: &str) {
    if value.is_empty() || row.iter().any(|f| f.value == value) {
        return;
    }
    row.push(crate::vector::FacetCount {
        value: value.to_string(),
        count: 0,
    });
}

#[derive(serde::Deserialize)]
pub(crate) struct UiSearchParams {
    #[serde(default)]
    pub(crate) q: String,
    #[serde(default)]
    pub(crate) tags: Option<String>,
    #[serde(default)]
    pub(crate) category: Option<String>,
    /// Absent while typing (vector order); `true` once the operator stops, reordering the rail.
    #[serde(default)]
    pub(crate) rerank: bool,
    /// Sent back so a typing burst folds into its own chain; see `Store::record_search`.
    #[serde(default)]
    pub(crate) fold: Option<String>,
    /// Off unless the link carries it (`/ui?explain=1`); through `query_flag` so `1` parses.
    #[serde(default, deserialize_with = "crate::web::api::query_flag")]
    pub(crate) explain: Option<bool>,
}

/// Function words match every chunk; highlighting them hides the terms that matched.
const STOPWORDS: [&str; 40] = [
    "a", "an", "the", "and", "or", "but", "if", "of", "to", "in", "on", "at", "by", "for", "with",
    "from", "into", "is", "are", "was", "were", "be", "been", "do", "does", "did", "how", "what",
    "when", "where", "why", "which", "that", "this", "it", "its", "my", "i", "you", "can",
];

/// Space separated for the client.
fn highlightable_terms(query: &str) -> String {
    crate::vector::sparse::tokenize(query)
        .into_iter()
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_tags(t: Option<String>) -> Vec<String> {
    t.map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// Past this the box holds something to keep, not search for; a bound on cost.
const MAX_QUERY_CHARS: usize = 2000;

pub(crate) async fn search_results(
    tenant: Tenant,
    // A `Tenant` is cached across requests and cannot carry this request's session.
    identity: crate::auth::Identity,
    headers: axum::http::HeaderMap,
    Query(p): Query<UiSearchParams>,
) -> UiResult<Response> {
    // Resolved once: both exits need it, and the subject is moved into the recall later.
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
    // An empty box, or one holding more than a query, is the idle state: searching a pasted chapter costs an embedding call.
    if p.q.trim().is_empty() || p.q.chars().count() > MAX_QUERY_CHARS {
        let mut t = idle_foot(&tenant, true, &headers).await?;
        // An unsearched long paste still gets its fate line.
        if !p.q.trim().is_empty() {
            t.echo = render_echo(&fate_echo(&tenant.core, &p.q, lang));
        }
        return Ok(HtmlTemplate(t).into_response());
    }

    // Handed to the client so highlighting never touches the sanitized HTML here.
    let terms = highlightable_terms(p.q.trim());
    // Kept because `p.q` is moved into the search; only the gap button reads it.
    let q = p.q.trim().to_string();
    // A typing burst folds into one entry here as in the log.
    if let Some(sess) = &identity.session {
        tenant.core.sittings.queried(
            sess,
            p.q.trim(),
            crate::store::now(),
            tenant.core.pursuit.idle_secs as i64,
        );
    }
    // Read into a local: a guard inside the call would be held across the await and is not `Send`.
    let cap = tenant
        .core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .per_source_cap;
    let explain = p.explain.unwrap_or(false);
    let (hits, outcome) = tenant
        .core
        .search_with(
            &SearchQuery {
                q: p.q,
                limit: 0,
                tags: split_tags(p.tags),
                category: p.category.filter(|c| !c.is_empty()),
                // Incremental: a prefix must not stamp what it happened to match.
                mark: false,
                include_deprecated: false,
                include_superseded: false,
                rerank: p.rerank,
                explain,
            },
            cap,
            // Scoped to the operator: two people typing at once are not spelling the same thing.
            crate::store::feedback::Door::Ui
                .by(tenant.user.subject)
                // Off unless `sitting.prime` is on; impossible without a session.
                .in_sitting(identity.session.clone())
                // So a burst folds into this page's chain, not another tab's.
                .folding_onto(p.fold.filter(|f| !f.is_empty())),
        )
        .await?;

    // An associated hit carries the id of its recaller, whose title is looked up among the ranked.
    let titles = ranked_titles(&hits);
    let (ranked, recalled): (Vec<_>, Vec<_>) = hits.into_iter().partition(|h| h.via.is_none());
    let mut results: Vec<RenderedResult> = ranked
        .into_iter()
        .enumerate()
        .map(|(i, h)| render_hit(i, h, &titles, explain))
        .collect();
    // Only over ranked rows, and best-effort: a marker must not cost the results.
    let next = match tenant
        .core
        .store
        .continuations_of(
            &results
                .iter()
                .map(|r| r.artifact_id.clone())
                .collect::<Vec<_>>(),
        )
        .await
    {
        Ok(next) => next,
        Err(e) => {
            tracing::warn!(error = %e, "could not read what these hits continue into");
            Default::default()
        }
    };
    mark_continuations(&mut results, &next);
    let associated: Vec<RenderedResult> = recalled
        .into_iter()
        .map(|h| render_hit(0, h, &titles, explain))
        .collect();
    let echo = render_echo(&fate_echo(&tenant.core, &q, lang));
    // Only when every result is loose, computed from `results` alone.
    let all_weak = !results.is_empty() && results.iter().all(|r| r.weak);
    // Read only where the rail would offer a gap.
    let (reading, held) = if (results.is_empty() && associated.is_empty()) || all_weak {
        (
            reading_a_capture(&tenant.core.store).await,
            // No gap on an empty base, but the event is still recorded: the first capture often answers it.
            tenant
                .core
                .store
                .held_brief()
                .await
                .map(|(c, _)| c > 0)
                .unwrap_or(true),
        )
    } else {
        (false, true)
    };
    let mut res = HtmlTemplate(ResultsTemplate {
        all_weak,
        reading,
        held,
        results,
        associated,
        terms,
        // From the outcome, not the request: a failed rerank answered in vector order.
        reranked: outcome.timing.reranked,
        event_id: outcome.event,
        echo,
        q,
    })
    .into_response();
    if let Ok(v) = format!(
        "embed;dur={}, total;dur={}",
        outcome.timing.embed_ms, outcome.timing.total_ms
    )
    .parse()
    {
        res.headers_mut().insert("server-timing", v);
    }
    Ok(res)
}

/// Untitled and borrowed-name hits are left out: the reader would look for a name nowhere on the page.
fn ranked_titles(
    hits: &[crate::core::search::SearchResult],
) -> std::collections::HashMap<String, String> {
    hits.iter()
        .filter(|h| h.via.is_none() && !h.borrowed_name)
        .filter_map(|h| Some((h.artifact_id.clone(), h.title.clone()?)))
        .collect()
}

/// Not the MCP form: a person reads why this row is above the next, not a list of stages.
pub(crate) fn why_ranked(e: &crate::core::explain::HitExplanation) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = &e.rerank
        && s.from > s.to
    {
        parts.push("moved up by the reranker".to_string());
    }
    if matches!(e.cap, crate::core::explain::CapEffect::Refilled) {
        parts.push("kept only because one source filled the list".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

pub(crate) fn render_hit(
    position: usize,
    h: crate::core::search::SearchResult,
    titles: &std::collections::HashMap<String, String>,
    // The flag gates rendering only.
    explain: bool,
) -> RenderedResult {
    let snippet = markdown::snippet(&h.text, 140);
    RenderedResult {
        artifact_id: h.artifact_id,
        // Empty, never "Untitled" or a borrowed heading; the row shows its snippet.
        title: match h.borrowed_name {
            true => String::new(),
            false => h.title.clone().unwrap_or_default(),
        },
        // The borrowed name as a prefixed qualifier under the snippet; dropped where it repeats the snippet.
        section: match h.borrowed_name {
            true => h
                .title
                .filter(|t| !snippet.starts_with(t.as_str()))
                .unwrap_or_default(),
            false => String::new(),
        },
        html: markdown::render(&h.text),
        snippet,
        category: h.category,
        tags: h.tags,
        corpus_id: h.corpus_id,
        // No rank on an associated hit: it did not compete for one.
        rank: if h.weak || h.via.is_some() {
            String::new()
        } else {
            format!("#{}", position + 1)
        },
        weak: h.weak,
        primed: h.primed,
        in_sitting: h.in_sitting,
        due_in: h.due_in.clone(),
        past_cliff: h.past_cliff,
        retired: h.retired,
        via_title: h.via.as_ref().and_then(|v| titles.get(v).cloned()),
        reason: h.reason.clone(),
        why_ranked: explain
            .then(|| h.explanation.as_ref().and_then(why_ranked))
            .flatten(),
        model_written: h.model_written,
        origin_count: h.origin_count,
        // Filled by `mark_continuations`: whether the continuation is on the page is a fact about the list.
        continues: false,
        continues_in: String::new(),
        disagrees: h
            .disagrees_with
            .iter()
            .map(|d| DisagreementRow {
                other_id: d.other_id.clone(),
                other_title: d
                    .other_title
                    .clone()
                    .unwrap_or_else(|| "another note".into()),
                day: crate::fmt::fmt_day(d.other_created_at),
                detail: d.detail.clone(),
            })
            .collect(),
    }
}

/// A continuation that placed is named by its rank; one that did not is announced.
/// A row with no rank is not a destination, so it falls back to the announcement.
pub(crate) fn mark_continuations(
    results: &mut [RenderedResult],
    next: &std::collections::HashMap<String, String>,
) {
    let ranks: std::collections::HashMap<&str, &str> = results
        .iter()
        .filter(|r| !r.rank.is_empty())
        .map(|r| (r.artifact_id.as_str(), r.rank.as_str()))
        .collect();
    let marks: Vec<(bool, String)> = results
        .iter()
        .map(|r| match next.get(&r.artifact_id) {
            None => (false, String::new()),
            Some(n) => match ranks.get(n.as_str()) {
                Some(rank) => (false, (*rank).to_string()),
                None => (true, String::new()),
            },
        })
        .collect();
    for (r, (continues, continues_in)) in results.iter_mut().zip(marks) {
        r.continues = continues;
        r.continues_in = continues_in;
    }
}

/// Where a label repeats in the list, the opening words are added; not where the label is unique, there is no opening, or the label is the opening.
fn disambiguate_labels(rows: &mut [QueueRow]) {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in rows.iter() {
        *counts.entry(r.label.as_str()).or_insert(0) += 1;
    }
    let collides: std::collections::HashSet<String> = counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(l, _)| l.to_string())
        .collect();
    for r in rows.iter_mut() {
        // The opening usually begins with the label it was lifted from; only the rest is worth the room.
        if let Some(rest) = r.opening.strip_prefix(r.label.as_str()) {
            let rest = rest
                .trim_start_matches([' ', ':', '·', '—', '-', ','])
                .trim();
            r.opening = rest.to_string();
        }
        // A field of its own, not appended: `.qtitle` is one `nowrap` line and would truncate it.
        if !(collides.contains(&r.label) && !r.opening.is_empty() && r.opening != r.label) {
            r.opening.clear();
        }
    }
}

async fn queue_fragment(tenant: Tenant) -> UiResult<Response> {
    let mut rows = Vec::new();
    let corpora = tenant.core.store.list_corpora(10, 0).await?;
    // Asked once per page (a three-way join on a polled fragment); failure is the empty map.
    let covered = tenant
        .core
        .store
        .gaps_covered_by_each(&corpora.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
        .await
        .unwrap_or_default();
    for s in corpora {
        let (resolved, total) = tenant.core.store.segment_progress(&s.id).await?;
        let progress = (total > 0 && resolved < total).then(|| format!("{resolved}/{total}"));
        // Terminal states; NeedsReview waits on a person.
        let in_flight = !matches!(
            s.status,
            CorpusStatus::Ready
                | CorpusStatus::Failed
                | CorpusStatus::NeedsReview
                | CorpusStatus::Partial
        );
        let low_coverage = crate::core::coverage::low_coverage(s.coverage);
        rows.push(QueueRow {
            progress,
            locatable: total > 0,
            coverage: s
                .coverage
                .map(|c| format!("{:.0}%", c * 100.0))
                .unwrap_or_else(|| "—".into()),
            low_coverage,
            // Until synthesis names it, a capture is called by its opening words; `unnamed` says a name is coming.
            opening: markdown::snippet(&s.raw_text, 60),
            label: corpus_label(s.title_hint.clone(), &s.raw_text, &s.origin),
            unnamed: s.title_hint.is_none() && in_flight,
            in_flight,
            settled: matches!(s.status, CorpusStatus::Ready),
            badge: status_badge(&s.status),
            status: s.status.as_str().to_string(),
            artifact_count: tenant.core.store.count_artifacts_for_corpus(&s.id).await?,
            created: fmt_time(s.created_at),
            covered: covered
                .get(&s.id)
                .map(|gs| gs.iter().map(|g| g.text.clone()).collect())
                .unwrap_or_default(),
            id: s.id,
        });
    }
    disambiguate_labels(&mut rows);
    let active = rows.iter().any(|r| r.in_flight);
    Ok(HtmlTemplate(QueueTemplate { rows, active }).into_response())
}

/// The rule lives in `markdown::stand_in_title`, so every surface names the same way.
pub(crate) fn title_of(c: &crate::store::artifacts::Chunk) -> String {
    // A passage and a note have no name of their own; see `Provenance::names_its_own_text`.
    if !c.provenance.names_its_own_text() {
        return String::new();
    }
    // Stored titles go through the same rule: synthesis can write markup into one.
    let name = match &c.title {
        Some(t) => crate::web::markdown::stand_in_title(t, 80),
        None => crate::web::markdown::stand_in_title(&c.text, 60),
    };
    // A body of only markup leaves nothing; the id is a poor name and a working one.
    if name.is_empty() {
        return c.id.clone();
    }
    name
}

/// Only whether a sentence finished, not whether the boundary was semantic.
/// A closing bracket or quote after the stop counts; a table row or list marker ends on its structure.
pub(crate) fn ends_mid_sentence(text: &str) -> bool {
    let t = text.trim_end();
    let t = t.trim_end_matches([')', ']', '"', '»', '\'', '“', '”']);
    match t.chars().last() {
        None => false,
        // A table row or a fence closes on its own punctuation.
        Some('|') | Some('`') => false,
        Some(c) => !matches!(c, '.' | '!' | '?' | ':' | ';' | '…'),
    }
}

#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundTemplate {}

impl NotFoundTemplate {
    fn section(&self) -> &'static str {
        ""
    }
}

/// Only for pages: an API, `/mcp`, asset or non-GET miss gets a plain 404, never a page or login redirect.
/// `Identity` is optional so `/api` stays a 404 without credentials, while a browser with no session still gets the 401.
pub async fn not_found(
    tenant: Option<Tenant>,
    method: axum::http::Method,
    uri: axum::http::Uri,
) -> Response {
    let path = uri.path();
    let machine = path.starts_with("/api/")
        || path.starts_with("/assets/")
        || path == "/mcp"
        || path.starts_with("/mcp/");
    if machine || method != axum::http::Method::GET {
        return (axum::http::StatusCode::NOT_FOUND, "not found").into_response();
    }
    let Some(_tenant) = tenant else {
        return crate::error::Error::Unauthorized.into_response();
    };
    not_found_page(axum::http::StatusCode::NOT_FOUND)
}

/// So a handler's `Error::NotFound` answers with the same page as an unrouted path; see `web::ui_error`.
pub(crate) fn not_found_page(status: axum::http::StatusCode) -> Response {
    match askama::Template::render(&NotFoundTemplate {}) {
        Ok(html) => (status, axum::response::Html(html)).into_response(),
        Err(_) => (status, "not found").into_response(),
    }
}

pub fn ui_router() -> Router<AppState> {
    Router::new()
        // `/` and `/ui` both open the workspace; an unmatched path never reaches authentication.
        .route("/ui/context", post(context_offer))
        .route("/ui/context/seen", post(context_seen))
        .route("/ui/queue", get(queue_fragment))
        // Installed PWAs and bookmarks may still hold /ui/browse. Takes an `Identity`, so a signed-out visitor is sent to sign in.
        .route(
            "/ui/browse",
            get(|_: Tenant| async { Redirect::to("/ui/capture") }),
        )
        // Straight to the page, not via the `/ui/ops` shim; takes an `Identity` so paths are not disclosed anonymously.
        .route(
            "/ui/housekeeping",
            get(|_: Tenant| async { Redirect::to("/ui/insights") }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::web::test_support::chunk_fixture;

    fn passage_fixture(title: Option<&str>, text: &str) -> crate::store::artifacts::Chunk {
        crate::store::artifacts::Chunk {
            provenance: crate::store::artifacts::Provenance::Passage,
            ..chunk_fixture(title, text)
        }
    }

    #[test]
    fn a_row_that_is_only_a_label_falls_back_to_the_text_not_to_a_name() {
        let c = passage_fixture(
            Some("Wiederherstellung geloeschter Eintraege"),
            "Der Vorgang setzt voraus, dass das Journal noch vollstaendig ist.",
        );
        let l = row_label(&c);
        assert!(
            l.text.starts_with("Der Vorgang setzt voraus"),
            "the row carried the section heading: {:?}",
            l.text
        );
        assert!(!l.named, "the opening of a text is not a name");
    }

    #[test]
    fn an_artifact_a_writer_named_keeps_its_name() {
        let c = chunk_fixture(Some("Wie ein Journal wiederhergestellt wird"), "body");
        assert_eq!(title_of(&c), "Wie ein Journal wiederhergestellt wird");
        let l = row_label(&c);
        assert_eq!(l.text, "Wie ein Journal wiederhergestellt wird");
        assert!(l.named);
    }

    async fn stored_passage(core: &crate::core::Core, title: &str, text: &str) -> (String, String) {
        let src = core.ingest(text, "web", None).await.unwrap();
        let p = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: text.into(),
                    title: Some(title.into()),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
                crate::store::artifacts::Provenance::Passage,
            )
            .await
            .unwrap();
        (src.id, p[0].id.clone())
    }

    #[tokio::test]
    async fn a_housekeeping_source_row_for_a_passage_is_still_a_link_you_can_read() {
        let core = crate::core::test_support::test_core().await;
        let (_, pid) = stored_passage(
            &core,
            "Kapitel 3",
            "Der Vorgang setzt voraus, dass das Journal noch vollstaendig ist.",
        )
        .await;
        let rows = source_rows(&core.store, "some-merge", &[pid]).await;
        assert!(
            rows[0].title.starts_with("Der Vorgang setzt voraus"),
            "the source row read {:?}",
            rows[0].title
        );
        assert!(!rows[0].named, "the opening of a text is not a name");
    }

    #[tokio::test]
    async fn a_row_whose_label_is_its_opening_does_not_say_it_twice() {
        let core = crate::core::test_support::test_core().await;
        let (_, pid) = stored_passage(
            &core,
            "Kapitel 3",
            "Der Vorgang setzt voraus, dass das Journal noch vollstaendig ist.",
        )
        .await;
        let rows = source_rows(&core.store, "some-merge", &[pid]).await;
        assert!(
            !rows[0].subtitle.contains("Der Vorgang setzt voraus"),
            "the opening stood twice, once under itself: {:?}",
            rows[0].subtitle
        );
    }

    fn queue_row_fixture(label: &str, opening: &str) -> QueueRow {
        QueueRow {
            label: label.into(),
            opening: opening.into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_opening_that_repeats_its_own_label_says_only_the_rest() {
        let mut rows = vec![
            queue_row_fixture(
                "HOCHSCHULE MITTWEIDA",
                "HOCHSCHULE MITTWEIDA Ein Verfahren zur Sicherung",
            ),
            queue_row_fixture(
                "HOCHSCHULE MITTWEIDA",
                "HOCHSCHULE MITTWEIDA Fachbereich Angewandte",
            ),
        ];
        disambiguate_labels(&mut rows);
        assert_eq!(rows[0].opening, "Ein Verfahren zur Sicherung");
        assert_eq!(rows[1].opening, "Fachbereich Angewandte");
    }

    #[test]
    fn a_disambiguated_row_shows_the_part_that_distinguishes_it() {
        let mut rows = vec![
            queue_row_fixture(
                "HOCHSCHULE MITTWEIDA",
                "Fachbereich Angewandte Computer- und Biowissenschaften",
            ),
            queue_row_fixture(
                "HOCHSCHULE MITTWEIDA",
                "Ein Verfahren zur Sicherung fluechtiger Daten",
            ),
            queue_row_fixture("SQLite und WAL", "Pragma-Abfragen"),
        ];
        disambiguate_labels(&mut rows);
        assert_eq!(
            rows[0].label, "HOCHSCHULE MITTWEIDA",
            "the label keeps its own name; the opening is said beside it"
        );
        assert!(!rows[0].opening.is_empty(), "nothing tells row 0 apart");
        assert!(
            rows[2].opening.is_empty(),
            "a unique label needs no opening beside it: {:?}",
            rows[2].opening
        );
        let html = askama::Template::render(&QueueTemplate {
            rows,
            active: false,
        })
        .unwrap();
        assert!(html.contains("qtitle-opening"), "{html}");
        assert!(html.contains("Fachbereich Angewandte"), "{html}");
    }

    #[test]
    fn a_sweep_stage_reads_as_words_and_keeps_its_identifier() {
        assert_eq!(sweep_label("arm_dedupe"), "Arming dedupe");
        assert_eq!(sweep_label("consolidate"), "Consolidating");
        assert_eq!(sweep_label("retention"), "Retention");
        assert_eq!(sweep_label("link_judge"), "Judging links");
        assert_eq!(sweep_label("some_new_sweep"), "some_new_sweep");
    }

    #[test]
    fn every_stage_the_queue_can_run_has_a_word_for_it() {
        // The map goes stale silently; this notices a stage added without a word.
        for stage in crate::store::jobs::Stage::ALL {
            let id = stage.as_str();
            assert_ne!(
                sweep_label(id),
                id,
                "no wording for the {id} stage — add one to `sweep_label`"
            );
        }
    }

    #[test]
    fn a_stored_title_that_carries_markup_is_shown_without_it() {
        let t = title_of(&chunk_fixture(
            Some("**Was nicht abgedeckt ist:** * Es werden keine"),
            "body",
        ));
        assert!(t.starts_with("Was nicht abgedeckt ist:"), "{t:?}");
        assert!(!t.contains("**"), "{t:?}");
        assert_eq!(
            title_of(&chunk_fixture(Some("# 3.4.2 FESTE MFT RECORDS"), "body")),
            "3.4.2 FESTE MFT RECORDS"
        );
        assert_eq!(
            title_of(&chunk_fixture(Some("LevelDB: Funktionsweise"), "body")),
            "LevelDB: Funktionsweise"
        );
    }

    #[test]
    fn a_passage_that_stops_mid_sentence_is_known_to_have_stopped() {
        assert!(ends_mid_sentence(
            "Die erste Vorkehrung ist der bereits vorgestellte Einsatz von"
        ));
        assert!(!ends_mid_sentence("Das ist der ganze Satz."));
        assert!(!ends_mid_sentence("Ist das der ganze Satz?"));
        assert!(!ends_mid_sentence("Ein Listenpunkt:"));
        assert!(!ends_mid_sentence("| ext4 | ja |"));
        assert!(!ends_mid_sentence(""));
    }

    #[test]
    fn the_copy_control_does_not_sit_on_top_of_the_passage() {
        let css = include_str!("../../assets/css/73-components.css");
        assert!(
            css.contains(".codewrap { position: relative; padding-top:"),
            "no room is reserved for the copy control"
        );
    }

    #[tokio::test]
    async fn a_search_with_nothing_open_leaves_the_grid_free_to_widen_the_rail() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/search?q=write+blocker").await;
        assert!(page.contains("regions-rail-focus-source"), "{page}");
        assert!(
            !page.contains("has-selection") && !page.contains("pane-open"),
            "a fresh search already claims something is open: {page}"
        );
    }

    #[test]
    fn the_artifact_pane_shows_a_stored_title_by_the_same_rule_as_the_rest() {
        assert_eq!(
            artifact_title(&chunk_fixture(
                Some("**Was nicht abgedeckt ist:** * Es werden keine"),
                "body"
            )),
            "Was nicht abgedeckt ist: * Es werden keine"
        );
    }

    #[test]
    fn an_artifact_whose_opening_is_only_markup_still_has_a_name() {
        let t = title_of(&chunk_fixture(None, "---"));
        assert!(!t.is_empty(), "a rail entry with no text is not a link");
        assert_eq!(t, "a", "the fixture's id");
    }

    #[test]
    fn the_artifact_pane_does_not_call_a_passage_chunk_fifty_six() {
        let t = artifact_title(&chunk_fixture(
            None,
            "Die digitale Forensik unterscheidet sich zusätzlich",
        ));
        assert!(!t.starts_with("Chunk"), "{t:?}");
        assert!(t.starts_with("Die digitale Forensik"), "{t:?}");
        assert_eq!(
            artifact_title(&chunk_fixture(Some("SQLite und WAL"), "body")),
            "SQLite und WAL"
        );
    }

    #[test]
    fn an_untitled_artifact_is_named_by_its_opening_not_by_its_first_sixty_bytes() {
        let t = title_of(&chunk_fixture(
            None,
            "Die digitale Forensik unterscheidet sich zusätzlich darin von einem Tatort",
        ));
        assert!(!t.ends_with("vo"), "cut mid-word: {t:?}");
        assert_eq!(
            title_of(&chunk_fixture(
                None,
                "- schneller Schreibzugriff (Änderungen vom Key auf Stapel) -"
            )),
            "schneller Schreibzugriff (Änderungen vom Key auf Stapel) -"
        );
        assert_eq!(
            title_of(&chunk_fixture(Some("LevelDB"), "body")),
            "LevelDB",
            "a real title is never replaced"
        );
    }
    use crate::web::test_support::{
        app_holding_something, app_recommending, app_session_and_core,
        app_session_and_core_with_feedback, app_with_cookie, app_with_embedded_corpus,
        app_with_session, artifacts, ask_over_sse, body_of, done_html, drain, flat, form, get_body,
        get_stream, hold_something, post_ask, pulled, searched_app, trigger_of,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn highlighting_skips_function_words_but_keeps_short_technical_terms() {
        let terms = super::highlightable_terms("how do i write an iso to a usb stick with dd");
        assert!(terms.contains("iso"));
        assert!(terms.contains("usb"));
        assert!(terms.contains("dd"), "short technical terms must survive");
        for noise in ["how", "the", " to ", " an ", " with "] {
            assert!(
                !format!(" {terms} ").contains(noise),
                "{noise} should not be highlighted"
            );
        }
    }

    #[tokio::test]
    async fn a_rail_entry_carries_the_chunk_id_it_links_to() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let hits = core
            .search(
                &crate::core::search::SearchQuery {
                    q: "alpha".into(),
                    limit: 0,
                    tags: vec![],
                    category: None,
                    mark: false,
                    include_deprecated: false,
                    include_superseded: false,
                    rerank: true,
                    explain: false,
                },
                crate::store::feedback::Door::Ui,
            )
            .await
            .unwrap();
        let r = super::render_hit(0, hits[0].clone(), &Default::default(), false);

        assert!(
            !r.artifact_id.is_empty(),
            "the rail needs a chunk id to link to"
        );
        assert!(!r.snippet.is_empty(), "the rail shows a plain-text snippet");
        assert!(
            !r.snippet.contains('<'),
            "the snippet must not carry markup"
        );
    }

    #[tokio::test]
    async fn a_failed_segment_is_picked_up_without_anyone_asking() {
        let core = crate::core::test_support::test_core().await;
        let out = core
            .ingest("first para\n\nsecond para", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        core.store
            .set_segment_state(
                &out.id,
                0,
                crate::store::segments::SegmentState::Failed,
                Some("boom"),
            )
            .await
            .unwrap();
        while core.store.claim_job().await.unwrap().is_some() {}

        assert_eq!(crate::jobs::reconcile::run(&core).await.unwrap(), 1);
        let mut found = false;
        let want = crate::jobs::window::unit_target(&out.id, 0);
        while let Some(j) = core.store.claim_job().await.unwrap() {
            if j.stage == crate::store::jobs::Stage::SegmentWindow && j.target_id == want {
                found = true;
            }
        }
        assert!(found, "nothing would ever retry the segment");
    }

    async fn app_for(core: crate::core::Core) -> (axum::Router, String) {
        app_with_cookie(core).await
    }

    async fn app_with_a_waiting_pair() -> (axum::Router, String) {
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        text: "The reindex job holds a file descriptor on the old mount.".into(),
                        title: Some("reindex holds an fd".into()),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "The reindex job holds an fd on the old mount.".into(),
                        title: Some("reindex holds an fd (again)".into()),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();
        core.store
            .record_pair_with_detail(&made[0].id, &made[1].id, 0.94, "near duplicate")
            .await
            .unwrap();
        app_for(core).await
    }

    #[tokio::test]
    async fn the_ask_door_fills_the_one_box_and_runs_nothing_on_arrival() {
        let core = crate::core::test_support::test_core().await;
        hold_something(&core).await;
        let (app, cookie) = app_for(core).await;
        let html = get(&app, "/ui/ask?q=why+did+the+reindex+fail", &cookie).await;
        assert!(
            html.contains("why did the reindex fail"),
            "the box carries the question: {html}"
        );
        assert!(
            !trigger_of(&html).contains("load"),
            "and nothing is searched for on the way in: {html}"
        );
        assert!(
            html.contains(r#"id="idle-foot""#),
            "the idle column says what the base holds: {html}"
        );
        for id in [
            "ask-live",
            "ask-result",
            "ask-status",
            "ask-stop",
            "ask-progress",
        ] {
            assert!(html.contains(id), "the driver's target {id} is on the page");
        }
    }

    #[test]
    fn a_long_answer_scrolls_inside_the_pinned_workspace() {
        let css = crate::web::assets::Assets::get("app.css").expect("app.css is embedded");
        let css = String::from_utf8(css.data.into_owned()).unwrap();
        let pinned = css
            .split_once(".regions-rail-focus-source { grid-template-rows:")
            .expect("the pinned block")
            .1;
        for target in ["#ask-live", "#ask-result"] {
            let rule = pinned
                .split_once(&format!(".pane > {target}"))
                .unwrap_or_else(|| panic!("{target} is not a scroller in the pinned block"))
                .1;
            let rule = &rule[..rule.find('}').unwrap()];
            assert!(rule.contains("overflow-y: auto"), "{target}: {rule}");
            assert!(rule.contains("min-height: 0"), "{target}: {rule}");
        }

        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        let token = js
            .split_once("addEventListener('token'")
            .expect("the driver handles token")
            .1;
        let token = &token[..token.find("addEventListener").unwrap()];
        assert!(
            token.contains("live.scrollTop = live.scrollHeight"),
            "the live answer does not follow its tail: {token}"
        );
    }

    #[tokio::test]
    async fn the_ask_door_is_absent_without_a_model() {
        let core = crate::core::test_support::test_core_without_ask().await;
        let (app, cookie) = app_for(core).await;
        let html = get(&app, "/ui", &cookie).await;
        assert!(
            !html.contains(r#"data-verb="ask""#),
            "no button where there is no model: {html}"
        );

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/ask")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "and the door is not there"
        );
    }

    #[tokio::test]
    async fn the_capture_door_fills_the_one_box_and_keeps_its_provenance() {
        let (app, cookie, _core, _html, id) = ask_recorded().await;

        let html = get(&app, &format!("/ui/capture?from_ask={id}"), &cookie).await;
        assert!(
            !trigger_of(&html).contains("load"),
            "and the answer in the box is not searched for: {html}"
        );
        assert!(html.contains(r#"id="box-form""#), "and it is the workspace");
        assert!(
            html.contains(r#"id="idle-foot""#),
            "which paints its idle column, not two empty ones: {html}"
        );
        assert!(
            html.contains(&format!(r#"name="from_ask" value="{id}""#)),
            "the ask rides the form as provenance: {html}"
        );
        assert!(
            html.contains(r#"<div id="kept-from">"#),
            "and it is retirable in one piece: {html}"
        );
        assert!(
            html.contains("Answer to"),
            "the question it answered is named"
        );

        let plain = get(&app, "/ui/capture", &cookie).await;
        assert!(plain.contains(r#"id="box-form""#), "still the workspace");
        assert!(
            !plain.contains("Kept from"),
            "an ordinary visit claims no provenance: {plain}"
        );
    }

    #[tokio::test]
    async fn the_file_control_offers_images_whether_or_not_vision_is_configured() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
        let html = get(&app, "/ui", &cookie).await;
        assert!(html.contains("image/*"), "the picker accepts images");

        let (app, cookie) =
            app_for(crate::core::test_support::test_core_without_vision().await).await;
        let html = get(&app, "/ui", &cookie).await;
        assert!(html.contains("image/*"));
        assert!(html.contains("read once a vision model is configured"));
    }

    #[tokio::test]
    async fn the_workspace_is_one_page_carrying_the_box_and_the_three_regions() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;

        for uri in ["/ui", "/ui/search"] {
            let html = get(&app, uri, &cookie).await;
            assert!(
                html.contains("regions-rail-focus-source"),
                "{uri}: the grid"
            );
            assert!(html.contains("name=\"q\""), "{uri}: the box");
            assert!(
                html.contains("hx-get=\"/ui/search/results\""),
                "{uri}: typing still asks the same endpoint"
            );
        }

        let html = get(&app, "/ui/search?q=volume+move", &cookie).await;
        assert!(html.contains("volume move"), "the box comes back filled");
        assert!(
            trigger_of(&html).contains("load"),
            "and the results are fetched on load: {html}"
        );
    }

    #[tokio::test]
    async fn a_paste_searches_because_typing_is_an_input_event() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_for(core.clone()).await;
        let html = get(&app, "/ui/search", &cookie).await;
        let trigger = trigger_of(&html);

        assert!(
            trigger.contains("input[!event.isComposing] changed delay:120ms from:textarea[name=q]"),
            "the box does not search on `input`: {trigger}"
        );
        assert!(
            !trigger.contains("keyup"),
            "`keyup` is back, and a mouse paste searches for nothing: {trigger}"
        );
    }

    #[tokio::test]
    async fn the_box_has_a_name_and_the_rail_says_what_it_found() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_for(core.clone()).await;
        let html = get(&app, "/ui/search", &cookie).await;

        assert!(
            html.contains(r#"aria-label="Search, ask, or capture""#),
            "the box has no accessible name: {html}"
        );
        assert!(
            html.contains("data-placeholder-narrow="),
            "nothing says what the box is for on a phone: {html}"
        );
        let head = html
            .split(r#"id="rail-head""#)
            .nth(1)
            .expect("the rail's heading");
        assert!(
            head.split('>')
                .next()
                .unwrap()
                .contains(r#"aria-live="polite""#),
            "a search announces nothing: {head}"
        );
    }

    #[test]
    fn the_idle_column_is_hidden_and_the_band_re_fetched_never_removed() {
        let js = include_str!("../../assets/app.js");
        assert!(
            js.contains("function showIdle(force)"),
            "nothing brings the column back"
        );
        assert!(
            js.contains("htmx.trigger(due, 'refresh')"),
            "the column comes back holding what was due a minute ago"
        );
        assert!(
            !js.contains("if (due) due.remove()"),
            "removing the band is the bug this pair of functions replaced"
        );
        let due = include_str!("templates/_due.html");
        assert!(
            due.contains(r#"hx-trigger="refresh"#),
            "and the band has nothing to answer that event with"
        );
        let ws = include_str!("templates/workspace.html");
        assert!(
            ws.contains(r#"<div id="idle""#) && ws.contains(r#"<div id="due""#),
            "the band lives inside the column, or hiding one does not hide the other"
        );
    }

    #[tokio::test]
    async fn a_small_paste_echoes_the_synthesis_it_will_get() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        let html = get(
            &app,
            "/ui/search/results?q=remind+me+tomorrow+to+send+the+invoice",
            &cookie,
        )
        .await;
        assert!(html.contains(r#"id="intent-echo""#), "{html}");
        assert!(
            html.contains("will be synthesized"),
            "it says its fate: {html}"
        );
        assert!(
            html.contains("structured artifacts"),
            "and what that means: {html}"
        );
    }

    #[tokio::test]
    async fn a_large_paste_echoes_its_verbatim_windows() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        let big = "filler+words+".repeat(400);
        let html = get(&app, &format!("/ui/search/results?q={big}"), &cookie).await;
        assert!(html.contains(r#"id="intent-echo""#), "{html}");
        assert!(html.contains("large paste"), "{html}");
        assert!(html.contains("verbatim"), "{html}");
    }

    #[tokio::test]
    async fn with_no_synthesizer_the_echo_promises_nothing_but_verbatim() {
        let core = crate::core::test_support::test_core_without_synthesizer().await;
        let echo = fate_echo(&core, "a short note", crate::infer::lang::Lang::En);
        assert_eq!(echo.kind, "stored as written");
        assert!(echo.detail.contains("no synthesizer"), "{}", echo.detail);
    }

    #[tokio::test]
    async fn the_echo_counts_the_windows_the_splitter_will_actually_make() {
        let core = crate::core::test_support::test_core().await;
        let lang = crate::infer::lang::Lang::En;
        let budget = crate::jobs::synthesize::segment_budget(&core, lang)
            .expect("a window")
            .max(1);
        let para = "filler words ".repeat(budget * 6 / 10);
        // Three, not ten: ten would cross `EXACT_SPLIT_BYTES`, where the echo estimates.
        let text = std::iter::repeat_n(para.trim(), 3)
            .collect::<Vec<_>>()
            .join("\n\n");
        let echo = fate_echo(&core, &text, lang);
        assert!(
            text.len() <= EXACT_SPLIT_BYTES,
            "the fixture has to stay under the bound the exact split is done below: {}",
            text.len()
        );
        assert_eq!(echo.kind, "large paste");
        let windows = crate::infer::split::split_into_segments(&text, &core.counter, budget).len();
        assert!(
            echo.detail.contains(&format!("in {windows} windows")),
            "the echo promises what the splitter does: {} against {windows}",
            echo.detail
        );
        assert!(
            windows > core.counter.count(&text).div_ceil(budget),
            "the fixture must be one the old arithmetic under-reported"
        );
    }

    #[tokio::test]
    async fn a_paste_too_big_to_split_twice_a_second_is_estimated_and_says_so() {
        let core = crate::core::test_support::test_core().await;
        let lang = crate::infer::lang::Lang::En;
        let budget = crate::jobs::synthesize::segment_budget(&core, lang)
            .expect("a window")
            .max(1);
        let para = "filler words ".repeat(budget * 6 / 10);
        let text = std::iter::repeat_n(para.trim(), 40)
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(text.len() > EXACT_SPLIT_BYTES);
        let echo = fate_echo(&core, &text, lang);
        assert_eq!(echo.kind, "large paste");
        let floor = core.counter.count(&text).div_ceil(budget);
        assert!(
            echo.detail.contains(&format!("at least {floor} windows")),
            "an estimate is offered as one: {}",
            echo.detail
        );
    }

    #[tokio::test]
    async fn the_examples_under_the_box_are_in_the_readers_language() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui")
                    .header("cookie", &cookie)
                    .header("accept-language", "de-DE,de;q=0.9,en;q=0.8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(
            html.contains("erinnere mich morgen"),
            "a German reader is shown German: {html}"
        );
        assert!(html.contains("chip-example"), "and it is pressable");
    }

    #[tokio::test]
    async fn the_idle_page_introduces_the_base_and_the_picker_is_reachable() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = app_for(core.clone()).await;

        let html = get(&app, "/ui", &cookie).await;
        assert!(
            html.contains("Paste a note"),
            "the empty base says what to do"
        );
        assert!(
            !html.contains("Nothing here yet"),
            "and does not say it a second time under the column"
        );
        assert!(
            html.contains(r#"<label class="oc-btn" id="drop""#),
            "the picker is in the verb row, not inside the hidden staged box"
        );
        let staged = html.split(r#"id="staged""#).nth(1).unwrap();
        assert!(
            !staged[..staged.find("</div>").unwrap()].contains("type=\"file\""),
            "and the staged box holds only the file, never the way to pick one"
        );

        let html = get(&app, "/ui", &cookie).await;
        assert!(
            html.contains(r#"id="idle""#),
            "the column exists as one element"
        );
        // Asserted on the template: the fixture seeds no facets, so a render has no chip row.
        let tpl = include_str!("templates/workspace.html");
        assert!(
            tpl.contains(
                r#"<span id="kind-row" class="kind-row"{% if idle_state %} hidden{% endif %}>"#
            ),
            "chips qualify a search, and an idle page has none"
        );
        assert!(
            !html.contains("or drop one anywhere on the page."),
            "the attach prose is on the button's title: {html}"
        );
        // `_box_hint.html` once: duplicate ids leave every copy after the first dead to out-of-band swaps.
        assert_eq!(
            html.matches(r#"id="box-hint""#).count(),
            1,
            "the hint under the box is in the document once: {html}"
        );
        assert_eq!(
            html.matches(r#"id="intent-echo""#).count(),
            1,
            "and so is the slot the echo swaps into: {html}"
        );
        let tpl = include_str!("templates/workspace.html");
        let hint = tpl
            .find(r#"{% include "_box_hint.html" %}"#)
            .expect("the include");
        assert!(hint < tpl.find(r#"<div id="idle""#).expect("the column"));

        core.ingest_capture(crate::core::ingest::Capture::new(
            "LevelDB tombstones survive compaction longer than the manual admits.",
            "ui",
        ))
        .await
        .unwrap();
        let html = get(&app, "/ui", &cookie).await;
        assert!(
            html.contains("artifact"),
            "the closing line counts what is held"
        );
        assert!(html.contains("last kept"), "and names what last went in");
        assert!(
            html.contains("LevelDB tombstones"),
            "in the operator's words"
        );
        assert!(!html.contains("Last captured"), "and only once: {html}");

        let frag = get(&app, "/ui/search/results?q=", &cookie).await;
        assert!(
            frag.contains(r#"id="idle-foot""#),
            "an empty query is idle again"
        );
        assert!(!frag.contains("No matches"), "not a verdict on the base");
    }

    #[tokio::test]
    async fn the_read_just_now_list_is_gone_but_the_sitting_is_not() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
        let html = get(&app, "/ui", &cookie).await;
        assert!(!html.contains("Read just now"), "the list is retired");

        // Covered behaviourally by `marking_a_carrier_marks_the_answer_right_and_updates_the_bar_out_of_band`.
    }

    #[tokio::test]
    async fn the_maintenance_surfaces_are_on_insights_and_a_waiting_pair_is_on_neither() {
        let (app, cookie) = app_with_a_waiting_pair().await;

        let insights = get(&app, "/ui/insights", &cookie).await;
        assert!(!insights.contains("Needs you"), "a pair asks nobody");
        assert!(
            !insights.contains("reindex holds an fd"),
            "and is not listed for a decision: {insights}"
        );
        assert!(
            insights.contains("Recent"),
            "the capture queue is on Insights"
        );
        assert!(
            insights.contains("/ui/queue"),
            "and it loads the same fragment it always did"
        );

        let capture = get(&app, "/ui/capture", &cookie).await;
        assert!(
            !capture.contains("Needs you"),
            "pairs left the capture page"
        );
        assert!(!capture.contains("/ui/queue"), "so did the queue");
    }

    #[tokio::test]
    async fn a_source_is_called_a_source_everywhere_a_person_reads_it() {
        let core = crate::core::test_support::test_core().await;
        let id = core
            .ingest_capture(crate::core::ingest::Capture::new(
                "LevelDB tombstones survive compaction longer than the manual admits.",
                "ui",
            ))
            .await
            .unwrap()
            .id;
        let (app, cookie) = app_for(core).await;

        let corpus = get(&app, &format!("/ui/corpora/{id}"), &cookie).await;
        assert!(
            !corpus.contains("Corpus — engram"),
            "the page a person reads does not say corpus"
        );
        assert!(
            corpus.contains("<title>LevelDB tombstones"),
            "the tab names this capture rather than its kind"
        );
        assert!(
            corpus.contains(r#"<h1 class="oc-text-page">LevelDB tombstones"#),
            "and so does the page"
        );
        assert!(
            !corpus.contains("Raw corpus"),
            "nor in the card over the text itself"
        );
        assert!(
            corpus.contains("/ui/corpora/"),
            "and the URL is untouched, because a path is not addressed to anyone"
        );

        let settings = get(&app, "/ui/settings", &cookie).await;
        assert!(!settings.contains(">Mint<"), "Mint is a word about coins");
        assert!(
            settings.contains(">Create token<"),
            "the button says what it does"
        );

        let ext = get(&app, "/extension/install", &cookie).await;
        assert!(
            !ext.contains("Housekeeping → API tokens"),
            "Housekeeping is retired and tokens were never there anyway"
        );
        assert!(
            ext.contains(r#"<a href="/ui/settings#tokens">API tokens</a>"#),
            "named where they are"
        );
    }

    #[tokio::test]
    async fn the_install_nudge_ships_hidden_and_app_js_gates_it() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;
        for uri in ["/ui", "/ui/settings"] {
            let html = get(&app, uri, &cookie).await;
            let nudge = html
                .split_once(r#"class="installnudge""#)
                .unwrap_or_else(|| panic!("no install nudge on {uri}: {html}"))
                .1;
            let tag = &nudge[..nudge.find('>').unwrap()];
            assert!(
                tag.contains("hidden"),
                "the nudge is not hidden on {uri}: {tag}"
            );
            assert!(nudge.contains("data-install-how"), "{uri}");
            assert!(
                nudge.contains("Add to Home Screen"),
                "the Safari route: {uri}"
            );
            assert!(nudge.contains("data-install"), "{uri}");
            assert!(nudge.contains("data-dismiss-install"), "{uri}");
        }

        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        let body = js
            .split_once("var INSTALL_KEY")
            .expect("app.js has no installNudge()")
            .1;
        let body = &body[..body.find("\n  }\n").unwrap()];
        for guard in [
            "display-mode: standalone",
            "navigator.standalone",
            "pointer: coarse",
            "max-width: 40rem",
            "7 * 24 * 60 * 60 * 1000",
            "engram.install-nudged",
        ] {
            assert!(
                body.contains(guard),
                "installNudge() lost `{guard}`: {body}"
            );
        }
        assert!(
            js.contains("    installNudge();\n"),
            "installNudge() is not called on load"
        );
        // Chrome fires the event once, early; the listener must be at script scope.
        let before = js.split_once("function installNudge() {").unwrap().0;
        assert!(
            before.contains("addEventListener('beforeinstallprompt'"),
            "beforeinstallprompt is not captured before load"
        );
    }

    #[tokio::test]
    async fn housekeeping_moved_to_insights_and_the_old_door_still_opens() {
        let (app, cookie) = app_for(crate::core::test_support::test_core().await).await;

        let html = get(&app, "/ui/insights", &cookie).await;
        assert!(
            html.contains("What the machine is doing"),
            "the maintenance section is there, under the name that says what \
             it holds rather than what a person might do about it"
        );

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/ops")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            res.headers().get("location").unwrap(),
            "/ui/insights",
            "the old door points at the new one"
        );
    }

    #[tokio::test]
    async fn a_link_derived_pair_never_claims_a_similarity_once_the_judge_has_settled_it() {
        // `detail` is overwritten by the dedupe judge, so provenance cannot be read from it once the pair settles.
        let core = crate::core::test_support::test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        text: "the first one".into(),
                        title: Some("first".into()),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "the second one".into(),
                        title: Some("second".into()),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();
        core.store
            .record_pair_with_detail(&made[0].id, &made[1].id, 0.0, "link")
            .await
            .unwrap();
        let id: i64 = sqlx::query_scalar!("SELECT id FROM artifact_pairs")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        core.store
            .set_pair_state(
                id,
                crate::store::pairs::PairState::Contradiction,
                Some("one says the opposite of the other"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();

        let (app, cookie) = app_for(core).await;
        let html = flat(&get(&app, "/ui/capture", &cookie).await);

        assert!(
            !html.contains("0% alike"),
            "a pair no cosine was ever measured for reports a measured similarity"
        );
    }

    #[tokio::test]
    async fn the_capture_page_offers_images_whether_or_not_vision_is_configured() {
        for core in [
            crate::core::test_support::test_core().await,
            crate::core::test_support::test_core_without_vision().await,
        ] {
            let (app, cookie) = app_for(core).await;
            let html = get(&app, "/ui/capture", &cookie).await;
            assert!(html.contains("image/*"), "picker accepts images");
        }
    }

    #[tokio::test]
    async fn the_capture_page_takes_a_pdf_whether_or_not_vision_is_configured() {
        for core in [
            crate::core::test_support::test_core().await,
            crate::core::test_support::test_core_without_vision().await,
        ] {
            let (app, cookie) = app_for(core).await;
            let html = get(&app, "/ui/capture", &cookie).await;
            assert!(html.contains("application/pdf"), "picker accepts PDFs");
        }
    }

    #[tokio::test]
    async fn a_pdf_waiting_to_be_extracted_is_called_a_document_in_the_queue() {
        let core = crate::core::test_support::test_core().await;
        core.ingest_pdf(crate::core::ingest::PdfCapture {
            bytes: include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec(),
            filename: Some("plan.pdf".into()),
            title_hint: None,
            note: None,
            lang: crate::infer::lang::Lang::default(),
        })
        .await
        .unwrap();

        let (app, cookie) = app_for(core).await;
        let html = get(&app, "/ui/queue", &cookie).await;
        assert!(
            html.contains(">document</span>"),
            "the row has no title to click: {html}"
        );
    }

    async fn app_with_a_learned_situation() -> (axum::Router, String, String) {
        app_with_a_learned_situation_over(
            "when the recycling centre is open",
            Some("recycling centre"),
            None,
        )
        .await
    }

    async fn app_with_a_learned_situation_over(
        text: &str,
        title: Option<&str>,
        provenance: Option<&str>,
    ) -> (axum::Router, String, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let aid = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: text.into(),
                    title: title.map(str::to_string),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id;
        if let Some(p) = provenance {
            sqlx::query!("UPDATE artifacts SET provenance = ? WHERE id = ?", p, &aid)
                .execute(&core.store.pool)
                .await
                .unwrap();
        }
        core.vectors
            .upsert(vec![crate::vector::VectorPoint {
                vector: vec![1.0; 8],
                sparse: Default::default(),
                payload: crate::vector::VectorPayload {
                    artifact_id: aid.clone(),
                    corpus_id: src.id.clone(),
                    text: text.into(),
                    title: title.map(str::to_string),
                    provenance: provenance.map(str::to_string),
                    ..Default::default()
                },
            }])
            .await
            .unwrap();

        let at = crate::store::now();
        let bundle = crate::core::context::Bundle {
            tz: Some("Europe/Berlin".into()),
            ..Default::default()
        };
        let v = crate::core::context::encode(at, &bundle, &core.recommend.weights);
        core.store
            .replace_context_clusters(
                &aid,
                &[crate::store::context::StoredCluster {
                    scope: Some("user-1".into()),
                    artifact_id: aid.clone(),
                    slot: 0,
                    centroid: v.clone(),
                    weight: 6.0,
                    events: 6,
                    last_at: at,
                    encoder_version: crate::core::context::encoder_version(&core.recommend.weights),
                    representative: serde_json::json!({ "at": at, "bundle": bundle }).to_string(),
                }],
            )
            .await
            .unwrap();
        core.vectors
            .set_context_vectors(&aid, vec![v])
            .await
            .unwrap();

        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        (app, cookie, aid)
    }

    #[tokio::test]
    async fn an_offer_with_neither_a_name_nor_a_snippet_is_not_made() {
        let (app, cookie, _aid) =
            app_with_a_learned_situation_over("![](figure.png)", None, Some("passage")).await;
        let res = app
            .oneshot(form(
                "/ui/context",
                &cookie,
                "bundle=%7B%22tz%22%3A%22Europe%2FBerlin%22%7D",
            ))
            .await
            .unwrap();
        let body = body_of(res).await;
        assert!(
            !body.contains("offer-filled"),
            "a card was drawn around nothing: {body}"
        );
        assert!(!body.contains("<a "), "a link with nothing in it: {body}");
        assert!(
            !body.contains("data-rec-id"),
            "an unreadable card must not be confirmable as seen: {body}"
        );
    }

    #[tokio::test]
    async fn a_page_view_is_recorded_even_when_nothing_is_offered() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let store = core.store.clone();
        let background = core.background.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let res = app
            .clone()
            .oneshot(form(
                "/ui/context",
                &cookie,
                "bundle=%7B%22tz%22%3A%22Europe%2FBerlin%22%7D",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        background.wait_idle().await;

        let rows = store.context_events_since(0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tz.as_deref(), Some("Europe/Berlin"));
        assert!(rows[0].local_hour.is_some(), "denormalised for the sweep");
        assert!(rows[0].weekday.is_some());
        assert_eq!(rows[0].scope.as_deref(), Some("user-1"));
        assert!(rows[0].bundle.contains("Europe/Berlin"));
    }

    #[tokio::test]
    async fn a_wide_bundle_is_stored_whole_and_read_by_no_block() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let store = core.store.clone();
        let background = core.background.clone();
        let weights = core.recommend.weights.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let narrow = r#"{"tz":"Europe/Berlin","platform":"Android"}"#;
        let wide = r#"{"tz":"Europe/Berlin","platform":"Android","place":"u33dc0","audio_route":"car","dnd":true}"#;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/context",
                &cookie,
                &format!("bundle={}", crate::web::pair::urlencode(wide)),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        background.wait_idle().await;

        let rows = store.context_events_since(0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].bundle.contains("u33dc0"), "stored whole");
        assert!(rows[0].bundle.contains("\"audio_route\":\"car\""));

        let at = 1_700_000_000;
        let a =
            crate::core::context::encode(at, &crate::core::context::parse_bundle(narrow), &weights);
        let b =
            crate::core::context::encode(at, &crate::core::context::parse_bundle(wide), &weights);
        assert_eq!(a, b, "no block reads the new fields");
    }

    #[tokio::test]
    async fn a_bundle_the_browser_could_not_build_does_not_break_the_page() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let store = core.store.clone();
        let background = core.background.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let res = app
            .clone()
            .oneshot(form("/ui/context", &cookie, "bundle=%7B%7Bnope"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "an empty bundle still works");
        background.wait_idle().await;
        assert_eq!(store.context_events_since(0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_area_is_not_rendered_when_the_faculty_is_off() {
        let core = crate::core::test_support::test_core().await;
        let store = core.store.clone();
        let background = core.background.clone();
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;

        let page = get(&app, "/ui/search", &cookie).await;
        assert!(!page.contains("/ui/context"), "no placeholder");

        let res = app
            .clone()
            .oneshot(form("/ui/context", &cookie, "bundle=%7B%7D"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        background.wait_idle().await;
        assert!(store.context_events_since(0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_placeholder_reserves_its_height_so_the_page_does_not_jump() {
        let (app, cookie, _store, _aid) = app_recommending().await;
        let page = get(&app, "/ui/search", &cookie).await;
        assert!(page.contains(r#"id="context-offer""#));
        assert!(page.contains(r#"hx-post="/ui/context""#));
        assert!(page.contains("data-send-context"));
        assert!(page.contains(r#"class="offer""#));
    }

    #[test]
    fn the_phone_does_not_hide_a_card_it_still_counts_as_seen() {
        let phone = include_str!("../../assets/css/78-phone.css");
        assert!(
            !phone.contains(".regions-rail-focus-source .region-bar .offer"),
            "the phone bar hides the offer while app.js still confirms it seen"
        );
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        assert!(
            js.contains("/ui/context/seen"),
            "the confirmation this rule must not outrun is gone"
        );
    }

    #[tokio::test]
    async fn the_offer_is_absent_from_a_page_that_already_carries_a_query() {
        // app.js removes the area only on a keystroke, so a restored query must not render it.
        let (app, cookie, _store, _aid) = app_recommending().await;
        let page = get(&app, "/ui/search?q=recycling", &cookie).await;
        assert!(
            !page.contains(r#"id="context-offer""#),
            "an offer beside results: {page}"
        );
        let page = get(&app, "/ui/search", &cookie).await;
        assert!(page.contains(r#"id="context-offer""#));
    }

    #[tokio::test]
    async fn a_confirmation_a_page_made_up_records_nothing() {
        // Both values come from whatever anyone posts and feed `offer_rates`' `GROUP BY rung`.
        let (app, cookie, store, aid) = app_recommending().await;

        for body in [
            format!("artifact_id={aid}&rung=excellent"),
            "artifact_id=no-such-artifact&rung=random".to_string(),
        ] {
            let res = app
                .clone()
                .oneshot(form("/ui/context/seen", &cookie, &body))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NO_CONTENT);
        }
        drain().await;

        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        assert!(
            rows.is_empty(),
            "a page wrote its own row into the hit rate: {rows:?}"
        );
    }

    #[tokio::test]
    async fn the_reason_line_is_markup_a_browser_will_not_rearrange() {
        // A `p` may hold only phrasing content; the parser would close it before `details`.
        let (app, cookie, _aid) = app_with_a_learned_situation().await;
        let body = crate::web::test_support::body_of(
            app.clone()
                .oneshot(form(
                    "/ui/context",
                    &cookie,
                    "bundle=%7B%22tz%22%3A%22Europe%2FBerlin%22%7D",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            body.contains("Offered because"),
            "no reason line at all: {body}"
        );
        assert!(body.contains(r#"<div class="muted offer-why">"#), "{body}");
        assert!(
            !body.contains("<p class=\"muted offer-why\">"),
            "the reason line must not be a paragraph: {body}"
        );
    }

    #[tokio::test]
    async fn what_was_offered_is_written_down_with_its_rung() {
        let (app, cookie, store, aid) = app_recommending().await;

        let res = app
            .clone()
            .oneshot(form("/ui/context", &cookie, "bundle=%7B%7D"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;
        assert!(body.contains(&aid), "{body}");
        assert!(
            !body.contains("offer-why"),
            "the card explains nothing: {body}"
        );
        assert!(!body.contains("Offered because"), "{body}");
        assert!(
            body.contains(r#"data-rec-rung="random""#),
            "the browser cannot say what it was shown: {body}"
        );
        drain().await;

        // Computing an offer is not showing one: nothing is recorded until the browser confirms.
        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        assert!(
            !rows.iter().any(|r| r.kind == "recommended_shown"),
            "an offer nobody has confirmed seeing is already counted: {rows:?}"
        );

        app.clone()
            .oneshot(form(
                "/ui/context/seen",
                &cookie,
                &format!("artifact_id={aid}&rung=random"),
            ))
            .await
            .unwrap();
        drain().await;

        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        let shown: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == "recommended_shown")
            .collect();
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].detail.as_deref().unwrap().contains("random"),
            "{:?}",
            shown[0].detail
        );
    }

    fn json_post(uri: &str, cookie: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("cookie", cookie)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn the_json_offer_is_the_fragments_offer_and_seen_is_what_counts_it() {
        let (app, cookie, store, aid) = app_recommending().await;

        let res = app
            .clone()
            .oneshot(json_post("/api/v1/context", &cookie, serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers().get("etag").is_none(),
            "an offer belongs to a moment; there is nothing to revalidate"
        );
        let body = crate::web::test_support::json_of(res).await;
        assert_eq!(body["offer"]["artifact_id"], aid.as_str(), "{body}");
        assert_eq!(body["offer"]["rung"], "random");
        assert!(body["offer"]["slot"].is_null());
        assert!(
            body["offer"]["label"]
                .as_str()
                .is_some_and(|l| !l.is_empty())
        );
        drain().await;
        let rows = store.interactions_between(0, i64::MAX).await.unwrap();
        assert!(!rows.iter().any(|r| r.kind == "recommended_shown"));

        for (rung, expect) in [("confident", 0), ("random", 1)] {
            let res = app
                .clone()
                .oneshot(json_post(
                    "/api/v1/context/seen",
                    &cookie,
                    serde_json::json!({ "artifact_id": aid, "rung": rung }),
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NO_CONTENT);
            drain().await;
            let rows = store.interactions_between(0, i64::MAX).await.unwrap();
            let shown = rows
                .iter()
                .filter(|r| r.kind == "recommended_shown")
                .count();
            assert_eq!(shown, expect, "after rung={rung}");
        }
    }

    #[tokio::test]
    async fn the_json_door_makes_no_offer_nobody_could_read_and_none_when_off() {
        let (app, cookie, _aid) =
            app_with_a_learned_situation_over("![](figure.png)", None, Some("passage")).await;
        let res = app
            .oneshot(json_post(
                "/api/v1/context",
                &cookie,
                serde_json::json!({ "tz": "Europe/Berlin" }),
            ))
            .await
            .unwrap();
        let body = crate::web::test_support::json_of(res).await;
        assert!(body["offer"].is_null(), "a card around nothing: {body}");

        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let res = app
            .oneshot(json_post("/api/v1/context", &cookie, serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = crate::web::test_support::json_of(res).await;
        assert!(body.get("offer").is_some_and(|o| o.is_null()), "{body}");
    }

    #[tokio::test]
    async fn ops_shows_shown_against_clicked_by_rung() {
        let mut core = crate::core::test_support::test_core().await;
        core.recommend.enabled = true;
        core.learn.enabled = true;
        let store = core.store.clone();
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
        let now = crate::store::now();
        for _ in 0..4 {
            store
                .record_recommendation(
                    &aid,
                    "recommended_shown",
                    r#"{"rung":"pattern"}"#,
                    Some("me"),
                    now,
                )
                .await
                .unwrap();
        }
        store
            .record_recommendation(
                &aid,
                "recommended_open",
                r#"{"rung":"pattern"}"#,
                Some("me"),
                now,
            )
            .await
            .unwrap();
        store
            .record_recommendation(
                &aid,
                "recommended_shown",
                r#"{"rung":"forgotten"}"#,
                Some("me"),
                now,
            )
            .await
            .unwrap();

        let rates = store.offer_rates(0).await.unwrap();
        assert_eq!(rates.len(), 2, "one row per rung: {rates:?}");
        let pattern = rates.iter().find(|r| r.rung == "pattern").unwrap();
        assert_eq!(pattern.shown, 4);
        assert_eq!(pattern.opened, 1);
        let forgotten = rates.iter().find(|r| r.rung == "forgotten").unwrap();
        assert_eq!(forgotten.shown, 1);
        assert_eq!(forgotten.opened, 0, "nobody took that one");

        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let page = get(&app, "/ui/insights", &cookie).await;
        assert!(page.contains("What was offered"), "no heading");
        assert!(page.contains("pattern"), "no rung");
        assert!(page.contains("forgotten"));
    }

    #[tokio::test]
    async fn ops_says_nothing_about_offers_when_the_faculty_is_off() {
        let core = crate::core::test_support::test_core().await;
        let (app, cookie) = crate::web::test_support::app_with_cookie(core).await;
        let page = get(&app, "/ui/insights", &cookie).await;
        assert!(!page.contains("What was offered"));
    }

    async fn get(app: &axum::Router, uri: &str, cookie: &str) -> String {
        get_body(app, cookie, uri).await
    }

    async fn app_recording_searches(pending: usize) -> (axum::Router, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        for i in 0..pending {
            core.store
                .record_search(
                    crate::store::feedback::NewEvent {
                        fold_onto: None,
                        query: format!("search number {i}"),
                        door: crate::store::feedback::Door::Ui,
                        scope: None,
                        filters: "{}".into(),
                        query_vec: vec![0.1, 0.2],
                        embed_model: "fake".into(),
                        candidates: vec![crate::store::feedback::NewCandidate {
                            artifact_id: format!("a{i}"),
                            score: 0.9,
                            similarity: Some(0.8),
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
        }
        let handle = core.clone();
        let out = app_with_cookie(core).await;
        hold_something(&handle).await;
        out
    }

    #[tokio::test]
    async fn every_link_to_the_review_screen_calls_it_what_it_calls_itself() {
        let (app, cookie) = app_recording_searches(3).await;
        for page in ["/ui/insights", "/ui/settings"] {
            let html = flat(&get(&app, page, &cookie).await);
            for stale in ["Judge some", "Judge them", ">Judge<"] {
                assert!(
                    !html.contains(stale),
                    "{page} still calls the review screen \"{stale}\""
                );
            }
        }
    }

    #[tokio::test]
    async fn the_nav_offers_no_judge_entry_even_while_searches_are_recorded() {
        let (app, cookie) = app_recording_searches(3).await;
        for page in ["/ui/search", "/ui/insights"] {
            let html = flat(&get(&app, page, &cookie).await);
            assert!(!html.contains("/ui/judge"), "{page} still links the deck");
        }
    }

    #[tokio::test]
    async fn the_search_page_offers_a_chip_for_what_the_collection_contains() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let html = flat(&get(&app, "/ui/search", &cookie).await);

        assert!(
            html.contains(r#"name="category" value="reference""#),
            "no category chip was rendered"
        );
        assert!(!html.contains(r#"name="tags""#), "the tag row is gone");
        assert!(
            html.contains(r#"name="category" value="" checked"#),
            "there must be a selected way back to every category"
        );
    }

    #[tokio::test]
    async fn a_deep_linked_filter_comes_back_selected() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let html = flat(&get(&app, "/ui/search?q=alpha&category=note", &cookie).await);
        assert!(
            html.contains(r#"name="category" value="note" checked"#),
            "the chip a link arrived with must render selected"
        );
        assert!(
            !html.contains(r#"name="category" value="" checked"#),
            "picking a category must deselect `all`"
        );
    }

    #[tokio::test]
    async fn a_deep_linked_filter_the_facets_do_not_list_still_gets_a_chip() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let html = flat(&get(&app, "/ui/search?q=alpha&category=recipe", &cookie).await);
        assert!(
            html.contains(r#"name="category" value="recipe" checked"#),
            "a filter outside the facet list must still render, and selected"
        );
        assert!(
            !html.contains(r#"name="category" value="" checked"#),
            "`all` must not look selected while a filter is applied"
        );
    }

    #[tokio::test]
    async fn the_search_page_renders_without_chips_when_there_is_nothing_to_narrow() {
        let (app, cookie) = app_with_session().await;
        let html = get(&app, "/ui/search", &cookie).await;
        assert!(html.contains(r#"name="q""#), "the search box must remain");
        assert!(
            !html.contains(r#"name="category""#),
            "an empty collection offers nothing to filter by"
        );
    }

    #[tokio::test]
    async fn a_chip_narrows_the_result_list() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let matching = get(
            &app,
            "/ui/search/results?q=alpha&category=reference",
            &cookie,
        )
        .await;
        let missing = get(
            &app,
            "/ui/search/results?q=alpha&category=procedure",
            &cookie,
        )
        .await;

        assert!(matching.contains("rail-item"), "the filter matched nothing");
        assert!(
            !missing.contains("rail-item"),
            "a category no artifact carries must return no results"
        );
    }

    #[test]
    fn a_loose_result_is_labelled_and_never_ranked() {
        let result = |weak: bool| crate::core::search::SearchResult {
            artifact_id: "a".into(),
            corpus_id: "s".into(),
            title: Some("t".into()),
            text: "body".into(),
            score: 0.5,
            weak,
            ..Default::default()
        };

        let loose = render_hit(0, result(true), &Default::default(), false);
        assert!(loose.weak);
        assert!(loose.rank.is_empty(), "a loose result was presented as #1");
        assert_eq!(
            render_hit(0, result(false), &Default::default(), false).rank,
            "#1"
        );

        let html = askama::Template::render(&ResultsTemplate {
            results: vec![loose],
            all_weak: true,
            ..Default::default()
        })
        .unwrap();
        assert!(html.contains("Nothing matches closely"), "{html}");
        assert!(!html.contains("#1"), "{html}");

        let mixed = askama::Template::render(&ResultsTemplate {
            results: vec![
                render_hit(0, result(false), &Default::default(), false),
                render_hit(1, result(false), &Default::default(), false),
                render_hit(2, result(true), &Default::default(), false),
            ],
            ..Default::default()
        })
        .unwrap();
        assert!(mixed.contains("3 results · 1 loose"), "{mixed}");
    }

    #[cfg(test)]
    fn row(id: &str, rank: &str) -> RenderedResult {
        RenderedResult {
            why_ranked: None,
            artifact_id: id.into(),
            title: String::new(),
            section: String::new(),
            html: String::new(),
            snippet: String::new(),
            category: None,
            tags: vec![],
            corpus_id: "s".into(),
            rank: rank.into(),
            weak: false,
            primed: false,
            in_sitting: false,
            due_in: None,
            past_cliff: false,
            retired: false,
            via_title: None,
            model_written: false,
            origin_count: 0,
            reason: None,
            continues: false,
            continues_in: String::new(),
            disagrees: Vec::new(),
        }
    }

    #[test]
    fn the_rail_prints_one_continuation_marker_or_the_other() {
        let mut named = row("a", "#1");
        named.continues_in = "#2".into();
        let mut offered = row("b", "#2");
        offered.continues = true;

        let html = askama::Template::render(&ResultsTemplate {
            results: vec![named, offered],
            ..Default::default()
        })
        .unwrap();

        assert!(html.contains("continues in #2"), "{html}");
        assert!(html.contains("continues in the next passage"), "{html}");
        assert_eq!(
            html.matches("rail-continues").count(),
            2,
            "one marker per row, and no row wearing both: {html}"
        );
    }

    #[test]
    fn a_row_that_continues_nowhere_prints_no_marker() {
        let html = askama::Template::render(&ResultsTemplate {
            results: vec![row("a", "#1")],
            ..Default::default()
        })
        .unwrap();

        assert!(!html.contains("rail-continues"), "{html}");
    }

    #[test]
    fn a_hit_whose_next_passage_also_placed_names_its_rank() {
        let mut rows = vec![row("a", "#1"), row("b", "#2")];
        let next = [("a".to_string(), "b".to_string())].into_iter().collect();
        super::mark_continuations(&mut rows, &next);

        assert_eq!(rows[0].continues_in, "#2");
        assert!(
            !rows[0].continues,
            "a hit whose continuation is on the page must not also offer to fetch it"
        );
    }

    #[test]
    fn a_hit_whose_next_passage_did_not_place_says_only_that_it_continues() {
        let mut rows = vec![row("a", "#1")];
        let next = [("a".to_string(), "elsewhere".to_string())]
            .into_iter()
            .collect();
        super::mark_continuations(&mut rows, &next);

        assert!(rows[0].continues);
        assert!(rows[0].continues_in.is_empty());
    }

    #[test]
    fn a_hit_at_the_end_of_its_document_is_marked_neither_way() {
        let mut rows = vec![row("a", "#1")];
        super::mark_continuations(&mut rows, &Default::default());

        assert!(!rows[0].continues);
        assert!(rows[0].continues_in.is_empty());
    }

    #[test]
    fn a_continuation_into_a_row_with_no_rank_is_not_named_by_rank() {
        let mut rows = vec![row("a", "#1"), row("b", "")];
        let next = [("a".to_string(), "b".to_string())].into_iter().collect();
        super::mark_continuations(&mut rows, &next);

        assert!(rows[0].continues_in.is_empty());
        assert!(
            rows[0].continues,
            "the continuation exists and must still be offered"
        );
    }

    #[test]
    fn a_result_with_no_title_of_its_own_is_given_no_heading() {
        let hit = |title: Option<&str>, via: Option<&str>| crate::core::search::SearchResult {
            artifact_id: "a".into(),
            corpus_id: "s".into(),
            title: title.map(str::to_string),
            text: "body".into(),
            score: 0.5,
            via: via.map(str::to_string),
            ..Default::default()
        };
        let titles = super::ranked_titles(&[hit(None, None)]);
        assert!(
            titles.is_empty(),
            "an untitled hit must not lend its name to what it recalled: {titles:?}"
        );
        let r = render_hit(0, hit(None, None), &titles, false);
        assert!(r.title.is_empty(), "{:?}", r.title);
        let html = askama::Template::render(&ResultsTemplate {
            results: vec![r],
            associated: vec![render_hit(0, hit(None, Some("a")), &titles, false)],
            ..Default::default()
        })
        .unwrap();
        assert!(!html.contains("Untitled"), "{html}");
        assert!(!html.contains("rail-title"), "{html}");
    }

    fn rendered(via: Option<&str>, reason: Option<&str>) -> RenderedResult {
        RenderedResult {
            why_ranked: None,
            artifact_id: "a1".into(),
            title: "The one that was recalled".into(),
            section: String::new(),
            html: String::new(),
            snippet: "a snippet".into(),
            category: None,
            tags: vec![],
            corpus_id: "c1".into(),
            rank: String::new(),
            weak: false,
            primed: false,
            in_sitting: false,
            due_in: None,
            past_cliff: false,
            retired: false,
            via_title: via.map(str::to_string),
            reason: reason.map(str::to_string),
            model_written: false,
            origin_count: 0,
            continues: false,
            continues_in: String::new(),
            disagrees: Vec::new(),
        }
    }

    #[test]
    fn an_offer_with_no_name_is_still_a_card_you_can_open() {
        let card = |title: &str| {
            askama::Template::render(&ContextTemplate {
                offer: Some(OfferView {
                    id: "a1".into(),
                    rec: String::new(),
                    title: title.into(),
                    snippet: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
                    kind: "pattern".into(),
                    rung: String::new(),
                    when: String::new(),
                    detail: String::new(),
                    blocks: String::new(),
                    slot: String::new(),
                }),
            })
            .unwrap()
        };
        let bare = card("");
        assert_eq!(
            bare.matches("<a ").count(),
            1,
            "the card has exactly one link: {bare}"
        );
        assert!(bare.contains("Der Vorgang setzt voraus"), "{bare}");
        assert!(bare.contains("name-opening"), "{bare}");
        assert_eq!(
            bare.matches("Der Vorgang setzt voraus").count(),
            1,
            "the snippet stood twice, once under itself: {bare}"
        );

        let named = card("Wie ein Journal steht");
        assert!(named.contains("Wie ein Journal steht"), "{named}");
        assert!(named.contains("Der Vorgang setzt voraus"), "{named}");
        assert!(!named.contains("name-opening"), "{named}");
    }

    #[test]
    fn the_rail_draws_the_cliff_once_and_greys_what_lies_past_it() {
        let mut above = rendered(None, None);
        above.rank = "#1".into();
        let mut past = rendered(None, None);
        past.rank = "#3".into();
        past.past_cliff = true;
        let mut also_past = past.clone();
        also_past.rank = "#4".into();
        let body = ResultsTemplate {
            results: vec![above.clone(), above.clone(), past, also_past],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert_eq!(
            body.matches("Relevance falls off here").count(),
            1,
            "{body}"
        );
        assert_eq!(body.matches("rail-past").count(), 2, "{body}");
        assert!(body.contains("#3") && body.contains("#4"), "{body}");
        let rule = body.find("Relevance falls off here").unwrap();
        assert!(body.find("#3").unwrap() > rule, "{body}");
        assert!(body.rfind("#1").unwrap() < rule, "{body}");

        let flat = ResultsTemplate {
            results: vec![above.clone(), above.clone(), above],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(!flat.contains("Relevance falls off here"), "{flat}");
        assert!(!flat.contains("rail-past"), "{flat}");
    }

    #[test]
    fn the_rail_shows_no_name_where_the_name_is_not_this_texts_own() {
        let hit = |borrowed: bool| crate::core::search::SearchResult {
            artifact_id: "a".into(),
            title: Some("Wiederherstellung geloeschter Eintraege".into()),
            text: "Der Vorgang setzt voraus, dass das Journal noch vollstaendig ist.".into(),
            borrowed_name: borrowed,
            ..Default::default()
        };
        let titles = std::collections::HashMap::new();
        assert_eq!(render_hit(0, hit(true), &titles, false).title, "");
        assert_eq!(
            render_hit(0, hit(false), &titles, false).title,
            "Wiederherstellung geloeschter Eintraege",
            "a name a writer gave this text is still shown"
        );
        let body = ResultsTemplate {
            results: vec![render_hit(0, hit(true), &titles, false)],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(
            !body.contains(r#"<span class="rail-title">"#),
            "the borrowed name stands where a name goes: {body}"
        );
        assert!(body.contains("Der Vorgang setzt voraus"), "{body}");
        assert!(
            body.contains(
                r#"<p class="rail-where">in Wiederherstellung geloeschter Eintraege</p>"#
            ),
            "{body}"
        );
        let css = include_str!("../../assets/app.css");
        assert!(
            !css.contains(".rail-title-corpus"),
            "the rule outlived its door"
        );
    }

    #[tokio::test]
    async fn the_results_name_what_recalled_an_associated_hit() {
        // Rendered directly: on a small base every artifact is already ranked, so nothing is left to recall.
        let template = ResultsTemplate {
            associated: vec![rendered(Some("Mounting E01 images"), None)],
            ..Default::default()
        };
        let body = template.render().unwrap();
        assert!(body.contains("Recalled by association"), "{body}");
        assert!(body.contains("seen together with"), "{body}");
        assert!(body.contains("Mounting E01 images"), "{body}");

        let judged = ResultsTemplate {
            associated: vec![rendered(
                Some("Mounting E01 images"),
                Some("the tool and its errors"),
            )],
            ..Default::default()
        };
        let body = judged.render().unwrap();
        assert!(body.contains("the tool and its errors"), "{body}");
        assert!(!body.contains("seen together with"), "{body}");
    }

    #[tokio::test]
    async fn a_reranked_fragment_says_it_was_refined() {
        let refined = ResultsTemplate {
            results: vec![rendered(Some("Mounting E01 images"), None)],
            reranked: true,
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(refined.contains("refined"), "{refined}");

        let fast = ResultsTemplate {
            results: vec![rendered(Some("Mounting E01 images"), None)],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(!fast.contains("refined"), "{fast}");
    }

    #[tokio::test]
    async fn an_unlinked_search_shows_no_association() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["alpha text"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();

        let body = get_body(&app, &cookie, "/ui/search/results?q=alpha").await;
        assert!(!body.contains("Recalled by association"), "{body}");
    }

    #[tokio::test]
    async fn the_search_fragment_takes_the_explain_flag_as_a_url_writes_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["alpha text"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();

        for spelling in ["explain=1", "explain=true", "explain=on", "explain"] {
            let uri = format!("/ui/search/results?q=alpha&{spelling}");
            let body = get_body(&app, &cookie, &uri).await;
            assert!(body.contains("rail-item"), "{uri} returned no rail: {body}");
        }
    }

    fn ranked(weak: bool) -> RenderedResult {
        RenderedResult {
            why_ranked: None,
            artifact_id: "r1".into(),
            title: "The ranked hit".into(),
            section: String::new(),
            html: String::new(),
            snippet: "a snippet".into(),
            category: None,
            tags: vec![],
            corpus_id: "c1".into(),
            rank: if weak { String::new() } else { "#1".into() },
            weak,
            primed: false,
            in_sitting: false,
            due_in: None,
            past_cliff: false,
            retired: false,
            via_title: None,
            reason: None,
            model_written: false,
            origin_count: 0,
            continues: false,
            continues_in: String::new(),
            disagrees: Vec::new(),
        }
    }

    #[test]
    fn an_association_cannot_make_the_answer_look_worse_than_it_was() {
        // An association must not move `all_weak` either way; both directions are checked.
        let weak_with_association = ResultsTemplate {
            results: vec![ranked(true)],
            associated: vec![rendered(Some("Mounting E01 images"), None)],
            all_weak: true,
            ..Default::default()
        };
        let body = weak_with_association.render().unwrap();
        assert!(
            body.contains("Nothing matches closely"),
            "an association hid a real warning: {body}"
        );

        let good_with_association = ResultsTemplate {
            results: vec![ranked(false)],
            associated: vec![rendered(Some("Mounting E01 images"), None)],
            ..Default::default()
        };
        let body = good_with_association.render().unwrap();
        assert!(
            !body.contains("Nothing matches closely"),
            "an association manufactured a warning: {body}"
        );
    }

    #[test]
    fn a_chosen_theme_beats_the_system_preference() {
        let css = include_str!("../../assets/app.css");
        assert!(
            css.contains(r#":root[data-theme="dark"]"#),
            "an explicit dark choice cannot beat a light system"
        );
        assert!(
            css.contains(r#":root:not([data-theme="light"])"#),
            "the system dark block does not yield to an explicit light choice"
        );
    }

    #[test]
    fn the_theme_is_applied_before_the_first_paint() {
        // The inline script must precede the stylesheet, or the wrong theme flashes on load.
        let layout = include_str!("templates/layout.html");
        let script = layout.find("js/theme.js").expect("no pre-paint script");
        let sheet = layout.find("url(\"app.css\")").expect("no stylesheet link");
        assert!(
            script < sheet,
            "the theme is applied after the stylesheet loads, which is the flash"
        );
    }

    #[test]
    fn headings_are_headings_and_labels_are_labels() {
        let css = include_str!("../../assets/app.css");
        assert!(
            css.contains(".label {"),
            "no .label class to carry the old h3 style"
        );
        assert!(
            !css.contains("h3 { font-size: 0.8125rem"),
            "h3 is still restyled as a label"
        );
        assert!(css.contains("--text-section:"), "the type scale is missing");

        let detail = include_str!("templates/_artifact_detail.html");
        assert!(
            detail.contains(r#"class="label pane-label""#),
            "the pane label does not compose .label"
        );
        let workspace = include_str!("templates/workspace.html");
        assert!(
            workspace.contains(r#"class="label facet-label""#),
            "the facet label does not compose .label"
        );
    }

    #[test]
    fn the_artifact_actions_carry_labels() {
        // Asserted against the template source: building an `ArtifactDetail` by hand is thirty lines for three words.
        let tpl = include_str!("templates/_artifact_detail.html");
        for word in ["Hide from results", "Delete"] {
            assert!(
                tpl.contains(&format!("<span>{word}</span>")),
                "the {word} control has no label"
            );
        }
        let rail = include_str!("templates/_results.html");
        assert!(
            !rail.contains("/delete"),
            "a result row must not delete what it is only showing"
        );
        assert!(
            include_str!("templates/_artifact.html").contains("<span>Delete</span>"),
            "the corpus page's artifact list stopped offering delete"
        );
    }

    #[test]
    fn the_open_rail_card_keeps_a_line_of_itself() {
        let css = include_str!("../../assets/app.css");
        assert!(
            !css.contains(r#".rail-item[aria-selected="true"] .rail-snippet { display: none; }"#),
            "the open card still erases its snippet"
        );
        // 0.55 over the dark base is likely under AA; a result past the cliff is still a result.
        assert!(
            !css.contains(".rail-past { opacity: 0.55; }"),
            "past-cliff results are still dimmed below the contrast floor"
        );
    }

    #[test]
    fn the_pane_controls_name_the_question_they_answer() {
        let html = include_str!("templates/_artifact_detail.html");
        assert!(
            !html.contains("<span>Still accurate</span>") && !html.contains("/verify"),
            "the pane still asks for a confirmation the verdict already gives"
        );
        assert!(
            html.contains("<span>Hide from results</span>"),
            "the hide control still says only `Hide`"
        );
        let bar = include_str!("templates/_search_verdict.html");
        assert!(bar.contains("Was this what you were looking for?"));
    }

    #[test]
    fn a_keystroke_hides_the_offer_and_does_not_destroy_it() {
        let js = include_str!("../../assets/app.js");
        let start = js.find("function hideIdle()").expect("no hideIdle");
        let body = &js[start..start + 260];
        assert!(
            !body.contains("area.remove()"),
            "hideIdle still destroys the offer instead of hiding it: {body}"
        );
        assert!(js.contains("function dropOffer()"));
        assert!(js.contains("if (offerDismissed) dropOffer();"));
    }

    #[test]
    fn every_page_anchors_to_the_same_left_edge() {
        let css = include_str!("../../assets/app.css");
        assert!(
            !css.contains("shell-wide"),
            "shell-wide still sets a per-page width"
        );
        assert!(
            css.contains(".regions-rail-focus-source"),
            "the three-up region tier is missing"
        );
    }

    #[test]
    fn colliding_capture_labels_get_told_apart() {
        let mut rows = vec![
            QueueRow {
                label: "HOCHSCHULE MITTWEIDA".into(),
                opening: "Kapitel 1 Einleitung".into(),
                ..Default::default()
            },
            QueueRow {
                label: "HOCHSCHULE MITTWEIDA".into(),
                opening: "Kapitel 5 Malware".into(),
                ..Default::default()
            },
            QueueRow {
                label: "Configure auditd".into(),
                opening: "auditctl -w /etc".into(),
                ..Default::default()
            },
        ];
        disambiguate_labels(&mut rows);
        assert_eq!(rows[0].label, "HOCHSCHULE MITTWEIDA");
        assert_eq!(rows[0].opening, "Kapitel 1 Einleitung");
        assert_eq!(rows[1].label, "HOCHSCHULE MITTWEIDA");
        assert_eq!(rows[1].opening, "Kapitel 5 Malware");
        assert_eq!(rows[2].label, "Configure auditd");
        assert!(rows[2].opening.is_empty());
    }

    #[test]
    fn a_collision_with_no_opening_words_is_left_alone() {
        let mut rows = vec![
            QueueRow {
                label: "document".into(),
                opening: String::new(),
                ..Default::default()
            },
            QueueRow {
                label: "document".into(),
                opening: String::new(),
                ..Default::default()
            },
        ];
        disambiguate_labels(&mut rows);
        assert_eq!(rows[0].label, "document");
        assert_eq!(rows[1].label, "document");
    }

    #[test]
    fn a_label_is_not_repeated_back_to_itself() {
        let mut rows = vec![
            QueueRow {
                label: "auditctl -w /etc".into(),
                opening: "auditctl -w /etc".into(),
                ..Default::default()
            },
            QueueRow {
                label: "auditctl -w /etc".into(),
                opening: "auditctl -w /etc".into(),
                ..Default::default()
            },
        ];
        disambiguate_labels(&mut rows);
        assert_eq!(rows[0].label, "auditctl -w /etc");
    }

    #[test]
    fn a_primed_hit_says_why_it_arrived() {
        let mut r = ranked(false);
        r.primed = true;
        let body = ResultsTemplate {
            results: vec![r],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(body.contains("rail-why"), "no provenance line: {body}");
        assert!(
            body.contains("opened, confirmed or cited more than the hits it passed"),
            "{body}"
        );
    }

    #[test]
    fn the_rail_sentence_names_a_cap_that_redistributed_nothing() {
        let e = crate::core::explain::HitExplanation {
            retrieved_rank: Some(3),
            cap: crate::core::explain::CapEffect::Refilled,
            ..Default::default()
        };
        let s = why_ranked(&e).expect("a refilled hit has something to say");
        assert!(
            s.contains("one source filled the list"),
            "the operator gets the consequence, not the mechanism: got {s:?}"
        );
    }

    #[test]
    fn a_hit_no_stage_touched_says_nothing() {
        let e = crate::core::explain::HitExplanation {
            retrieved_rank: Some(0),
            cap: crate::core::explain::CapEffect::Kept,
            ..Default::default()
        };
        assert!(
            why_ranked(&e).is_none(),
            "a quiet stage renders nothing; a row of no-ops is noise"
        );
    }

    #[test]
    fn an_ordinary_hit_explains_nothing() {
        let body = ResultsTemplate {
            results: vec![ranked(false)],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(
            !body.contains("rail-why"),
            "an ordinary hit explained itself: {body}"
        );
    }

    #[test]
    fn a_primed_hit_says_it_moved_up() {
        let mut r = ranked(false);
        r.primed = true;
        let body = ResultsTemplate {
            results: vec![r],
            ..Default::default()
        }
        .render()
        .unwrap();
        assert!(body.contains("moved up"), "{body}");
    }

    #[test]
    fn status_maps_to_the_right_status_kind() {
        use crate::store::corpora::CorpusStatus::*;
        assert_eq!(status_badge(&Ready), "success");
        assert_eq!(status_badge(&Partial), "warning");
        assert_eq!(status_badge(&Failed), "danger");
        assert_eq!(status_badge(&Raw), "none");
        assert_eq!(status_badge(&Embedding), "none");
    }

    #[tokio::test]
    async fn every_ui_route_requires_a_session() {
        let (app, _) = app_with_session().await;
        for uri in [
            "/ui/capture",
            "/ui/search",
            "/ui/search/results?q=x",
            "/ui/browse",
            "/ui/queue",
            "/ui/corpora/abc",
            "/ui/ask",
            "/ui/insights",
        ] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::SEE_OTHER, "{uri} was unprotected");
            let go: String = url::form_urlencoded::byte_serialize(uri.as_bytes()).collect();
            assert_eq!(
                res.headers().get("location").unwrap(),
                &format!("/auth/login?go={go}"),
                "{uri} did not send an unauthenticated page load to sign in"
            );
        }
        for uri in [
            "/ui/capture",
            "/ui/ops/tokens",
            "/ui/corpora/abc/delete",
            "/ui/corpora/abc/reprocess",
            "/ui/ops/artifacts/abc/deprecate",
            "/ui/ask",
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .method("POST")
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from("name=x&text=y&q=z"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "POST {uri} was unprotected"
            );
        }
    }

    #[tokio::test]
    async fn a_flagged_capture_says_what_it_resembles_and_that_it_is_searchable() {
        let (app, cookie, core) = app_session_and_core().await;
        let body: String = (0..200)
            .map(|i| format!("step {i} run the mount command and read its output"))
            .collect::<Vec<_>>()
            .join("\n");
        core.ingest(&body, "web", None).await.unwrap();

        let edited = body
            .replacen("step 7 ", "step seven ", 1)
            .replace(' ', "+")
            .replace('\n', "%0A");
        let res = app
            .oneshot(form("/ui/capture", &cookie, &format!("text={edited}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = flat(&body_of(res).await).to_lowercase();
        assert!(
            html.contains("% like") && html.contains("a kept source"),
            "the flagged capture rendered as an ordinary one: {html}"
        );
        assert!(
            html.contains("stored and searchable"),
            "it must say the capture is not held: {html}"
        );
        assert!(
            !html.contains("waiting on a decision"),
            "a flagged capture waits on nobody: {html}"
        );
    }

    #[tokio::test]
    async fn capturing_text_stores_it_and_the_idle_line_says_so() {
        let (app, cookie, core) = app_session_and_core().await;
        let res = app
            .clone()
            .oneshot(form("/ui/capture", &cookie, "text=a+new+procedure"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.trim().is_empty(),
            "the press still writes over the box it cleared: {html}"
        );

        let stored = core.store.list_corpora(10, 0).await.unwrap();
        assert_eq!(stored.len(), 1, "the capture itself still landed");
        let idle = get_body(&app, &cookie, "/ui").await;
        assert!(
            idle.contains(&format!("/ui/corpora/{}", stored[0].id)),
            "and nothing names what was last kept: {idle}"
        );
    }

    #[tokio::test]
    async fn capturing_the_same_text_twice_says_so() {
        let (app, cookie) = app_with_session().await;
        for _ in 0..1 {
            app.clone()
                .oneshot(form("/ui/capture", &cookie, "text=a+new+procedure"))
                .await
                .unwrap();
        }
        let res = app
            .oneshot(form("/ui/capture", &cookie, "text=a+new+procedure"))
            .await
            .unwrap();
        assert!(
            body_of(res).await.to_lowercase().contains("already stored"),
            "a duplicate paste must say why nothing new appeared"
        );
    }

    #[tokio::test]
    async fn capture_takes_only_text() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=another+one&title=ignored",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_deep_link_runs_its_query_instead_of_only_filling_the_box() {
        let (app, cookie) = app_with_session().await;
        let page = |uri: &'static str| {
            let app = app.clone();
            let cookie = cookie.clone();
            async move {
                let res = app
                    .oneshot(
                        Request::builder()
                            .uri(uri)
                            .header("cookie", cookie)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res.status(), StatusCode::OK);
                body_of(res).await
            }
        };

        let linked = page("/ui/search?q=mounting").await;
        assert!(
            linked.contains("load"),
            "the deep link never asks for its own results"
        );
        assert!(
            !trigger_of(&page("/ui/search").await).contains("load"),
            "an empty box has nothing to search for"
        );
    }

    #[test]
    fn the_spinner_says_which_pass_is_in_flight() {
        let js = include_str!("../../assets/app.js");
        assert!(
            js.contains("'reranking…' : 'retrieving…'"),
            "the two passes are no longer named apart in app.js"
        );
        assert!(
            js.contains("getElementById('search-spinner')"),
            "app.js no longer reaches the element the template renders"
        );
        let tpl = include_str!("templates/workspace.html");
        assert!(
            tpl.contains(r#"id="search-spinner""#),
            "the element the words are written into is gone from the template"
        );
    }

    #[tokio::test]
    async fn search_results_are_a_fragment_not_a_page() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=mounting")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(!html.contains("<html"), "results must be a fragment");
    }

    #[tokio::test]
    async fn a_rate_limited_search_is_busy_rather_than_a_bad_gateway() {
        let mut core = crate::core::test_support::test_core().await;
        core.embedder = std::sync::Arc::new(
            crate::infer::fake::FakeEmbedder::new(core.embedder.dim())
                .busy_for_the_first(usize::MAX),
        );
        let (app, cookie) = app_for(core).await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=mounting")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_busy_search_keeps_the_results_it_was_typed_over() {
        let js = include_str!("../../assets/app.js");
        assert!(
            js.contains("status === 503 && busyNote("),
            "nothing tells a busy search apart from a broken one in app.js"
        );
        let note = js
            .split_once("function busyNote(")
            .expect("busyNote is gone")
            .1;
        let body = &note[..note.find("\n  }").expect("busyNote's end")];
        assert!(
            body.contains("insertBefore"),
            "the note must go above the results, not over them: {body}"
        );
        assert!(
            !body.contains("textContent = ''"),
            "busyNote empties the rail, which is the whole thing it exists not to do: {body}"
        );
    }

    #[tokio::test]
    async fn a_box_with_no_search_reranker_never_claims_refinement() {
        let (app, cookie, core) = app_session_and_core().await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        while crate::jobs::run_one(&core).await.unwrap() {}

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=mounting&rerank=true")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.contains("result-count"),
            "the fragment must actually carry results for this test to mean \
             anything: {html}"
        );
        assert!(
            !html.contains("refined"),
            "a box with no search reranker claimed a refinement: {html}"
        );
    }

    #[tokio::test]
    async fn a_fragment_whose_rerank_failed_never_claims_refinement() {
        let core = crate::core::test_support::test_core_with_failing_reranker().await;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        while crate::jobs::run_one(&handle).await.unwrap() {}

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=mounting&rerank=true")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.contains("result-count"),
            "the fragment must actually carry results for this test to mean \
             anything: {html}"
        );
        assert!(
            !html.contains("refined"),
            "the rerank call failed, so the order is vector order and the \
             fragment must not claim the reranker confirmed it: {html}"
        );
    }

    #[tokio::test]
    async fn an_empty_result_set_never_claims_refinement() {
        let (core, _reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        let (app, cookie) = app_with_cookie(core).await;

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=nothing+matches+this&rerank=true")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            !html.contains("refined"),
            "no rerank call was made for an empty answer, so nothing was \
             confirmed: {html}"
        );
    }

    #[tokio::test]
    async fn a_rerank_that_actually_ran_earns_the_refined_tick() {
        let (core, _reranker) = crate::core::test_support::test_core_counting_reranked_docs().await;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        app.clone()
            .oneshot(form("/ui/capture", &cookie, "text=mounting+an+image"))
            .await
            .unwrap();
        while crate::jobs::run_one(&handle).await.unwrap() {}

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=mounting&rerank=true")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.contains("refined"),
            "the reranker ran and confirmed the order; the tick is earned: {html}"
        );
    }

    #[tokio::test]
    async fn rendered_chunk_html_is_sanitized() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=%3Cscript%3Ealert(1)%3C%2Fscript%3E+plus+some+words",
            ))
            .await
            .unwrap();
        let state_app = app.clone();
        let _ = state_app;

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=words")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(
            !html.contains("<script"),
            "unsanitized chunk reached the page: {html}"
        );
    }

    #[tokio::test]
    async fn an_empty_query_returns_an_empty_fragment_not_an_error() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "typing then clearing the box must not error"
        );
    }

    #[tokio::test]
    async fn a_box_holding_a_document_is_idle_rather_than_a_query() {
        let (app, cookie) = app_with_session().await;
        let long = "answer+".repeat(MAX_QUERY_CHARS / 4);
        let frag = get(&app, &format!("/ui/search/results?q={long}"), &cookie).await;
        assert!(
            frag.contains(r#"id="idle-foot""#),
            "a pasted document must land on the idle state:\n{frag}"
        );
        assert!(!frag.contains("No matches"), "and not as a verdict on it");
    }

    #[tokio::test]
    async fn the_queue_lists_recent_captures_and_polls_only_while_busy() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();

        let body = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            body.contains("alpha line"),
            "a capture nothing has read yet is called by its opening words, \
             which is what tells two of them apart"
        );
        assert!(body.contains("sse:queue"), "work in flight keeps listening");

        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let body = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            body.contains("alpha line"),
            "the row is called by the name capture derived"
        );
        assert!(
            !body.contains("sse:queue"),
            "an idle queue stops listening for work"
        );
        assert!(
            body.contains("captured from:body"),
            "an idle queue still listens, or a capture pasted onto it never \
             appears without a reload"
        );
    }

    #[tokio::test]
    async fn a_capture_that_stopped_without_finishing_says_which_way() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core.ingest("alpha line", "web", None).await.unwrap();

        for (status, badge) in [
            (
                crate::store::corpora::CorpusStatus::Failed,
                "oc-status--danger",
            ),
            (
                crate::store::corpora::CorpusStatus::NeedsReview,
                "oc-status--warning",
            ),
            (
                crate::store::corpora::CorpusStatus::Partial,
                "oc-status--warning",
            ),
        ] {
            let name = status.as_str();
            core.store.set_corpus_status(&out.id, status).await.unwrap();
            let body = get_body(&app, &cookie, "/ui/queue").await;
            assert!(
                body.contains(badge) && body.contains(name),
                "{name} renders no status of its own"
            );
            assert!(
                !body.contains("0 artifacts"),
                "{name} reads as a finished capture that produced nothing"
            );
            assert!(
                !body.contains("every 3s"),
                "{name} waits on a person or on nobody; polling it changes nothing"
            );
        }
    }

    #[tokio::test]
    async fn insights_offers_no_decision_however_many_pairs_are_open() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(
            &core,
            &[
                "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n",
            ],
        )
        .await;
        for w in ids.chunks(2) {
            core.store.record_pair(&w[0], &w[1], 0.9).await.unwrap();
        }

        let body = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            !body.contains("/supersede"),
            "a Keep button is back: {body}"
        );
        assert!(!body.contains("more waiting"), "a queue is back: {body}");
    }

    #[tokio::test]
    async fn browse_redirects_to_capture() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/browse")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(res.headers()["location"], "/ui/capture");
    }

    #[tokio::test]
    async fn ops_says_what_the_sweeps_did_and_shows_the_runs() {
        let (app, cookie, core) = app_session_and_core().await;
        for _ in 0..2 {
            core.store
                .record_sweep_run(
                    "associate",
                    crate::store::now(),
                    "ok",
                    r#"{"events":0,"verdicts":0,"forgotten":206,"reopened":0,"armed":0}"#,
                )
                .await
                .unwrap();
        }
        core.store
            .record_sweep_run(
                "consolidate",
                crate::store::now(),
                "failed",
                r#"{"error":"the endpoint was down"}"#,
            )
            .await
            .unwrap();

        let html = get(&app, "/ui/insights", &cookie).await;
        assert!(
            html.contains("412 links forgotten"),
            "the last day did not add the runs up: {html}"
        );
        assert!(html.contains("1 run failed"), "a failed run went unsaid");
        assert!(
            html.contains("the endpoint was down"),
            "the history did not say why a run failed"
        );
    }

    #[tokio::test]
    async fn ops_shows_queue_state() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(html.contains("artifacts,"), "the counts are still stated");
        assert!(html.contains("Nothing done on its own yet"));
        assert!(!html.contains("<h3>Hidden as stale</h3>"));
    }

    #[tokio::test]
    async fn ops_says_how_many_links_there_are_and_how_many_are_named() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let ids = artifacts(&core, &["alpha text", "something else entirely"]).await;
        core.store
            .bump_link(&ids[0], &ids[1], 5.0, Some("q"), 30.0, crate::store::now())
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(page.contains("1 links"), "{page}");
    }

    #[tokio::test]
    async fn ops_reports_what_is_retrying_rather_than_asking_for_a_click() {
        let (app, cookie, core) = app_session_and_core().await;
        core.store
            .enqueue(crate::store::jobs::Stage::Embed, "artifact", "a1")
            .await
            .unwrap();
        let job = core.store.claim_job().await.unwrap().unwrap();
        core.store
            .fail_job(job.id, 9, "endpoint down")
            .await
            .unwrap();

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(html.contains("Retrying"), "{html}");
        assert!(html.contains("endpoint down"));
        assert!(
            !html.contains("Re-synthesize segment"),
            "the review queue is still a to-do list"
        );
    }

    #[tokio::test]
    async fn the_counts_say_what_they_count() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            page.contains("jobs") || page.contains("No jobs queued"),
            "a job count must not read as an artifact count: {page}"
        );
    }

    #[tokio::test]
    async fn both_reversals_are_called_the_same_thing() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["kept one", "hidden one"]).await;
        core.store
            .set_superseded_by(&ids[1], Some(&ids[0]))
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(!page.contains("Put it back"), "{page}");
        assert!(!page.contains("Undo merge"), "{page}");
        assert!(page.contains(">Undo<"), "{page}");
    }

    #[tokio::test]
    async fn identically_titled_rows_are_told_apart() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["Windows Update-Typen", "Windows Update-Typen"]).await;
        core.store
            .set_superseded_by(&ids[1], Some(&ids[0]))
            .await
            .unwrap();

        let page = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            page.contains("body of Windows Update-Typen"),
            "a row has to say which artifact it is, and two can share a title: {page}"
        );
    }

    #[tokio::test]
    async fn the_installation_lives_on_its_own_page() {
        let (app, cookie) = app_with_session().await;

        let settings = get_body(&app, &cookie, "/ui/settings").await;
        assert!(settings.contains("API tokens"), "{settings}");
        assert!(
            settings.contains("Install the browser extension"),
            "{settings}"
        );

        let ops = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            !ops.contains("API tokens"),
            "housekeeping is about the corpus: {ops}"
        );
        assert!(!ops.contains("Browser extension"), "{ops}");
    }

    #[tokio::test]
    async fn both_pages_are_reachable_once_capture_stops_being_one() {
        let (app, cookie) = app_with_session().await;

        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(page.contains("/ui/insights"), "{page}");

        let insights = get_body(&app, &cookie, "/ui/insights").await;
        assert!(insights.contains("/ui/settings"), "{insights}");
    }

    #[tokio::test]
    async fn the_result_list_says_how_many_and_keeps_debug_timing_off_the_page() {
        let (app, cookie) = app_with_embedded_corpus().await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/search/results?q=alpha")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            res.headers().contains_key("server-timing"),
            "the measurement moved to a header, it was not dropped"
        );
        let frag = body_of(res).await;
        assert!(frag.contains("result-count"), "the count is stated: {frag}");
        assert!(
            frag.contains(r#"hx-swap-oob="innerHTML:#rail-head""#),
            "the count does not reach the rail heading: {frag}"
        );
        assert!(
            !frag.contains("embed ") && !frag.contains("server-timing"),
            "timing is not operator-facing: {frag}"
        );
    }

    #[tokio::test]
    async fn a_settled_row_states_its_count_and_mentions_coverage_only_when_it_is_short() {
        let (app, cookie, core) = app_session_and_core().await;
        let out = core
            .ingest("alpha line\n\nbravo line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();

        let frag = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            frag.contains("artifacts"),
            "a settled row states its count: {frag}"
        );
        assert!(
            !frag.contains(" covered"),
            "a fully covered row announced a number that is the same on every row: {frag}"
        );
        assert!(
            !frag.contains("oc-status--warning"),
            "the warning is carried by colour on the number, not by a status: {frag}"
        );
    }

    async fn ask_recorded() -> (axum::Router, String, crate::core::Core, String, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let html = done_html(&ask_over_sse(&app, &cookie, "what+is+alpha").await);
        assert_eq!(
            handle.store.ask_stats().await.unwrap().asked,
            1,
            "the UI ask was not recorded"
        );
        let id: String = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM ask_events LIMIT 1"#)
            .fetch_one(&handle.store.pool)
            .await
            .unwrap();
        (app, cookie, handle, html, id)
    }

    #[tokio::test]
    async fn editing_an_answer_first_fills_the_capture_box_and_stores_nothing() {
        let (app, cookie, core, html, id) = ask_recorded().await;
        assert!(
            html.contains(&format!("/ui/capture?from_ask={id}")),
            "the answer offers no way to edit it before keeping it: {html}"
        );
        let before = core.store.list_corpora(100, 0).await.unwrap().len();

        let page = get_body(&app, &cookie, &format!("/ui/capture?from_ask={id}")).await;
        let answer = core.store.ask_event(&id).await.unwrap().unwrap().answer;
        assert!(
            page.contains(answer.trim()),
            "the answer is not in the box: {page}"
        );
        assert!(
            page.contains(&format!(r#"name="from_ask" value="{id}""#)),
            "the ask does not ride the form, so nothing would record where the text came from: {page}"
        );
        assert_eq!(
            core.store.list_corpora(100, 0).await.unwrap().len(),
            before,
            "opening the capture page must store nothing"
        );
    }

    #[tokio::test]
    async fn keeping_an_answer_stores_it_and_queues_it_like_any_capture() {
        let (app, cookie, core, html, id) = ask_recorded().await;
        assert!(
            html.contains(&format!("/ui/ask/{id}/keep")),
            "the answer offers no way to keep it in place: {html}"
        );
        let answer = core.store.ask_event(&id).await.unwrap().unwrap().answer;

        let res = app
            .clone()
            .oneshot(form(&format!("/ui/ask/{id}/keep"), &cookie, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;

        let (corpus_id, origin, metadata): (String, String, String) = sqlx::query!(
            r#"SELECT id AS "id!", origin, metadata FROM corpora WHERE raw_text = ?"#,
            &answer
        )
        .fetch_one(&core.store.pool)
        .await
        .map(|r| (r.id, r.origin, r.metadata))
        .unwrap();
        assert_eq!(origin, "ask", "a kept answer must not read as a paste");
        let meta: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(meta["ask"]["event_id"], id.as_str());
        assert_eq!(meta["ask"]["question"], "what is alpha");
        assert!(
            meta["ask"]["artifact_ids"]
                .as_array()
                .is_some_and(|a| !a.is_empty()),
            "the artifacts the answer was written from are the provenance: {meta}"
        );
        assert!(
            body.contains(&format!("/ui/corpora/{corpus_id}")),
            "the operator is not told where the answer went: {body}"
        );
        let queued: i64 =
            sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE target_id = ?", &corpus_id)
                .fetch_one(&core.store.control.pool)
                .await
                .unwrap();
        assert!(queued > 0, "a kept answer was stored but never processed");
    }

    #[tokio::test]
    async fn a_kept_answer_is_stored_as_a_paste_that_records_the_question() {
        let (app, cookie, core, _html, id) = ask_recorded().await;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                &format!("text=edited+by+hand&from_ask={id}"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let corpus: (String, String) =
            sqlx::query!("SELECT origin, metadata FROM corpora WHERE raw_text = 'edited by hand'")
                .fetch_one(&core.store.pool)
                .await
                .map(|r| (r.origin, r.metadata))
                .unwrap();
        assert_eq!(corpus.0, "ask", "a kept answer must not read as a paste");
        let meta: serde_json::Value = serde_json::from_str(&corpus.1).unwrap();
        assert_eq!(meta["ask"]["event_id"], id.as_str());
        assert_eq!(meta["ask"]["question"], "what is alpha");
        assert!(
            meta["ask"]["artifact_ids"]
                .as_array()
                .is_some_and(|a| !a.is_empty()),
            "the artifacts the answer was written from are the provenance: {meta}"
        );
    }

    #[tokio::test]
    async fn a_kept_answer_whose_ask_is_gone_is_stored_as_an_ordinary_paste() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let res = app
            .clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=an+answer+whose+question+expired&from_ask=no-such-ask",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let row: (String, String) = sqlx::query!(
            "SELECT origin, metadata FROM corpora WHERE raw_text = 'an answer whose question expired'"
        )
        .fetch_one(&core.store.pool)
        .await
        .map(|r| (r.origin, r.metadata))
        .unwrap();
        assert_eq!(
            row.0, "web",
            "a claim of model authorship must not outlive the evidence for it"
        );
        let meta: serde_json::Value = serde_json::from_str(&row.1).unwrap();
        assert!(meta.get("ask").is_none(), "{meta}");
    }

    #[tokio::test]
    async fn an_ordinary_capture_still_records_itself_as_one() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        let res = app
            .clone()
            .oneshot(form("/ui/capture", &cookie, "text=typed+by+a+person"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let origin: String =
            sqlx::query_scalar!("SELECT origin FROM corpora WHERE raw_text = 'typed by a person'")
                .fetch_one(&core.store.pool)
                .await
                .unwrap();
        assert_eq!(origin, "web");
    }

    #[tokio::test]
    async fn the_answer_page_offers_a_verdict_when_the_question_was_recorded() {
        let (_app, _cookie, _core, html, id) = ask_recorded().await;
        assert!(html.contains(&format!("/ui/ask/{id}/verdict")), "{html}");
        assert!(html.contains("Nothing here"), "{html}");
        assert!(html.contains(&format!("/ui/ask/{id}/carried")), "{html}");
    }

    #[tokio::test]
    async fn the_answer_page_offers_no_verdict_when_feedback_is_off() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=alpha+para%0A%0Abeta+para",
            ))
            .await
            .unwrap();
        let html = done_html(&ask_over_sse(&app, &cookie, "what+is+alpha").await);
        assert!(!html.contains("/verdict"), "{html}");
    }

    #[tokio::test]
    async fn a_verdict_is_recorded_and_can_be_undone() {
        let (app, cookie, core, _, id) = ask_recorded().await;
        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ask/{id}/verdict"),
                &cookie,
                "verdict=wrong",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bar = body_of(res).await;
        assert!(bar.contains("wrong") && bar.contains("Undo"), "{bar}");
        assert_eq!(
            core.store.ask_event(&id).await.unwrap().unwrap().verdict,
            Some(crate::store::asks::AskVerdict::Wrong)
        );

        let bar = body_of(
            app.clone()
                .oneshot(form(
                    &format!("/ui/ask/{id}/verdict"),
                    &cookie,
                    "verdict=none",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(bar.contains("Nothing here"), "the buttons are back: {bar}");
        assert!(
            core.store
                .ask_event(&id)
                .await
                .unwrap()
                .unwrap()
                .verdict
                .is_none()
        );
    }

    #[tokio::test]
    async fn marking_a_carrier_marks_the_answer_right_and_updates_the_bar_out_of_band() {
        let (app, cookie, core, _, id) = ask_recorded().await;
        let res = app
            .clone()
            .oneshot(form(&format!("/ui/ask/{id}/carried"), &cookie, "n=1"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_of(res).await;
        assert!(
            html.contains("hx-swap-oob"),
            "the verdict bar must follow the toggle: {html}"
        );
        assert!(html.contains("right"), "{html}");
        assert_eq!(
            html.matches(r#"id="ask-verdict""#).count(),
            1,
            "the swapped-in bar carries the id twice: {html}"
        );
        let ev = core.store.ask_event(&id).await.unwrap().unwrap();
        assert_eq!(ev.verdict, Some(crate::store::asks::AskVerdict::Right));
        assert!(ev.citations[0].carried);
    }

    #[tokio::test]
    async fn judging_an_unknown_question_is_not_found() {
        let (app, cookie) = app_with_session().await;
        let res = app
            .oneshot(form("/ui/ask/nope/verdict", &cookie, "verdict=right"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unanswered_question_is_put_to_nobody_on_insights() {
        let (app, cookie, core) = app_session_and_core_with_feedback().await;
        for q in ["how do I mount an E01", "mounting E01 images read only"] {
            let id = core
                .store
                .record_ask(crate::store::asks::NewAsk {
                    question: q.into(),
                    filters: "{}".into(),
                    query_vec: vec![1.0; 8],
                    embed_model: core.embedder.model().to_string(),
                    answer: "Not in the knowledge base.".into(),
                    abstained: true,
                    ..Default::default()
                })
                .await
                .unwrap();
            core.store
                .judge_ask(&id, crate::store::asks::AskVerdict::NothingHere)
                .await
                .unwrap();
        }
        for when in ["before", "after"] {
            if when == "after" {
                crate::jobs::gaps::sweep(&core).await.unwrap();
            }
            let page = get_body(&app, &cookie, "/ui/insights").await;
            assert!(!page.contains("Knowledge gaps"), "{when}: {page}");
            assert!(!page.contains("mount an E01"), "{when}: {page}");
            assert!(!page.contains("/ui/gaps/"), "{when}: {page}");
        }
    }

    #[tokio::test]
    async fn a_capture_that_answered_something_says_so_on_its_row() {
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
        let gap = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "how do I mount an E01".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0; crate::core::test_support::TEST_DIM],
                    embed_model: core.embedder.model().to_string(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        core.store
            .judge(
                &gap,
                crate::store::feedback::Verdict::Gap,
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();
        core.store
            .cover_gap(
                crate::store::gaps::GapKind::Search,
                &gap,
                &src.id,
                &a,
                0.71,
                crate::store::gaps::CoveredBy::Distance,
            )
            .await
            .unwrap();

        let queue = get_body(&app, &cookie, "/ui/queue").await;
        assert!(
            queue.contains("how do I mount an E01"),
            "the row does not say what this capture answered: {queue}"
        );
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(
            !page.contains(&format!("gap-search-{gap}")),
            "a covered gap is still open: {page}"
        );
    }

    #[tokio::test]
    async fn a_question_the_operator_arrived_with_is_never_overwritten() {
        let (app, cookie, core) = app_session_and_core().await;
        hold_something(&core).await;
        get_body(&app, &cookie, "/ui/search/results?q=something%20else").await;

        let ask = get_body(&app, &cookie, "/ui/ask?q=the%20one%20I%20clicked").await;
        assert!(ask.contains("the one I clicked"), "{ask}");
        assert!(!ask.contains("something else"), "{ask}");
    }

    #[tokio::test]
    async fn a_cold_sitting_renders_no_rail_at_all() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/search").await;
        assert!(!page.contains("Read just now"), "{page}");
    }

    #[tokio::test]
    async fn housekeeping_counts_only_the_pursuits_the_gap_list_still_holds() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        let core = handle;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let art = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "how to mount an E01".into(),
                    title: Some("Mounting an E01".into()),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        let mut ids = Vec::new();
        for q in ["pursued one", "pursued two"] {
            let p = core
                .store
                .insert_pursuit(
                    1,
                    &[q.to_string()],
                    &[],
                    Some((
                        &[1.0; crate::core::test_support::TEST_DIM],
                        core.embedder.model(),
                    )),
                )
                .await
                .unwrap();
            core.store
                .close_pursuit(&p, "unsatisfied", "nothing strong was engaged", 2)
                .await
                .unwrap();
            ids.push(p);
        }

        let both = get_body(&app, &cookie, "/ui/insights").await;
        assert!(both.contains("2 went unanswered"), "{both}");

        core.store
            .cover_gap(
                crate::store::gaps::GapKind::Pursuit,
                &ids[0],
                &src.id,
                &art,
                0.8,
                crate::store::gaps::CoveredBy::Distance,
            )
            .await
            .unwrap();

        let one = get_body(&app, &cookie, "/ui/insights").await;
        assert!(one.contains("1 went unanswered."), "{one}");
        assert!(!one.contains("on the list above"), "{one}");
    }

    #[tokio::test]
    async fn the_capture_page_shows_no_gaps_block_when_feedback_is_off() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/capture").await;
        assert!(!page.contains("Knowledge gaps"), "{page}");
    }

    #[tokio::test]
    async fn the_capture_button_comes_after_every_field_it_submits() {
        let (app, cookie) = app_with_session().await;
        let page = get_body(&app, &cookie, "/ui/capture").await;

        assert!(
            !page.contains(r#"name="note""#),
            "the note is the box, not a field of its own: {page}"
        );
        let box_at = page
            .find(r#"name="q""#)
            .expect("the one box is on the page");
        let staged = page
            .find(r#"id="staged""#)
            .expect("a file waits to be sent rather than going on arrival");
        let button = page
            .find(r#"data-verb="capture""#)
            .expect("the capture verb is there");
        assert!(
            box_at < button && staged < button,
            "the button must come after everything it sends: {page}"
        );

        // Pinned to the fields the search takes, or every keystroke carries a filename; `tz` belongs to the capture path.
        assert!(
            page.contains(r#"hx-params="q,category,rerank,explain,fold""#),
            "{page}"
        );
    }

    #[tokio::test]
    async fn the_nav_is_the_same_width_on_every_page() {
        let (app, cookie) = app_with_session().await;
        for uri in ["/ui/capture", "/ui/search", "/ui/insights"] {
            let page = get_body(&app, &cookie, uri).await;
            let bar = page.find(r#"class="oc-topbar""#).expect("a top bar");
            let shell = page.find(r#"class="shell"#).expect("a shell");
            assert!(
                bar < shell,
                "the nav must sit outside the shell, or it inherits that page's \
                 measure and moves as you navigate: {uri}"
            );
        }
    }

    #[tokio::test]
    async fn the_ask_page_prefills_a_question_from_the_query_string() {
        let (app, cookie) = app_holding_something().await;
        let page = get_body(&app, &cookie, "/ui/ask?q=mount+an+E01").await;
        assert!(page.contains(">mount an E01</textarea>"), "{page}");
    }

    #[tokio::test]
    async fn ask_renders_an_answer_with_citations() {
        let (app, cookie) = app_with_session().await;
        app.clone()
            .oneshot(form(
                "/ui/capture",
                &cookie,
                "text=alpha+para%0A%0Abeta+para",
            ))
            .await
            .unwrap();
        let html = done_html(&ask_over_sse(&app, &cookie, "what+is+alpha").await);
        assert!(html.contains("Answer"), "{html}");
    }

    #[tokio::test]
    async fn the_answer_page_badges_and_marks_a_literal_no_excerpt_carries() {
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
            reply: Some("First run `wipefs --all /dev/sdX`, then read alpha.".into()),
        }));
        let (app, cookie) = app_with_cookie(core).await;
        let html = done_html(&ask_over_sse(&app, &cookie, "what+is+alpha").await);
        assert!(
            html.contains("literal no excerpt supports"),
            "no badge: {html}"
        );
        assert!(
            html.contains(r#"<mark class="unsupported">wipefs --all /dev/sdX</mark>"#),
            "the invented command is not marked in the prose: {html}"
        );
    }

    async fn app_session_and_core_with_an_embedded_base()
    -> (axum::Router, String, crate::core::Core) {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let out = core
            .ingest("alpha line\n\nbravo line\n\ncharlie line", "web", None)
            .await
            .unwrap();
        crate::jobs::synthesize::segment_all(&core, &out.id).await;
        crate::jobs::embed::run_corpus(&core, &out.id)
            .await
            .unwrap();
        let handle = core.clone();
        let (app, cookie) = app_with_cookie(core).await;
        (app, cookie, handle)
    }

    #[tokio::test]
    async fn an_ask_handoff_id_cannot_be_used_twice() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let id = post_ask(&app, &cookie, "what+is+alpha").await;
        let first = get_stream(&app, &cookie, &id).await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = get_stream(&app, &cookie, &id).await;
        assert_eq!(second.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unknown_handoff_id_is_not_found() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let res = get_stream(&app, &cookie, "nope").await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_stream_route_refuses_a_visitor_without_a_session() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let id = post_ask(&app, &cookie, "what+is+alpha").await;
        let res = get_stream(&app, "", &id).await;
        assert_ne!(
            res.status(),
            StatusCode::OK,
            "an unsigned-in visitor streamed"
        );
    }

    #[tokio::test]
    async fn the_stream_ends_with_a_done_event_carrying_rendered_html() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let id = post_ask(&app, &cookie, "what+is+alpha").await;
        let res = get_stream(&app, &cookie, &id).await;
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let body = body_of(res).await;
        assert!(body.contains("event: done"), "{body}");
        assert!(
            done_html(&body).contains("<div class=\"md\">"),
            "the done event carries the rendered fragment: {body}"
        );
    }

    #[tokio::test]
    async fn the_page_names_each_script_by_a_hash_of_that_script() {
        use sha2::Digest;
        let (app, cookie) = app_holding_something().await;
        let page = get_body(&app, &cookie, "/ui/ask").await;
        for name in ["app.js", "app.css", "vendor/htmx.min.js"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets")
                .join(name);
            let hash = sha2::Sha256::digest(std::fs::read(&path).unwrap());
            let want = format!("/assets/{name}?v={}", hex::encode(&hash[..6]));
            assert!(page.contains(&want), "{want} missing: {page}");
        }

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(crate::web::assets::url("app.js"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_of(res).await.contains("askDriver"), "not the driver");
    }

    #[test]
    fn pressing_ask_reveals_the_regions_the_answer_is_written_into() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let handler = js
            .split_once(r#"closest('[data-verb="ask"]')"#)
            .expect("the ask click handler is gone")
            .1;
        let handler = &handler[..handler.find("addEventListener").unwrap_or(handler.len())];
        assert!(
            handler.contains("hideIdle();"),
            "the ask handler does not reveal the rail and the pane: {handler}"
        );
        assert!(
            handler.find("hideIdle();") < handler.find("live.hidden"),
            "the regions must be revealed before the answer is written into them"
        );
    }

    #[test]
    fn the_countdown_is_whole_seconds() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let f = js
            .split_once("function spanWords(secs) {")
            .expect("spanWords is gone")
            .1;
        let f = &f[..f.find("\n  }").expect("spanWords does not end")];
        assert!(
            f.contains("Math.floor(secs)"),
            "the seconds branch renders a fraction: {f}"
        );
    }

    #[test]
    fn the_stream_driver_closes_the_event_source_on_every_exit() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let stop = js
            .split_once("function stop() {")
            .expect("the driver has no stop()")
            .1;
        assert!(
            stop[..stop.find('}').unwrap()].contains("source.close()"),
            "stop() no longer closes the EventSource: {stop}"
        );

        // Close before the payload is touched, or a malformed one leaves the stream open.
        let done = js
            .split_once("addEventListener('done'")
            .expect("the driver does not handle done")
            .1;
        let done = &done[..done.find("addEventListener").unwrap_or(done.len())];
        assert!(
            done.contains("stop();"),
            "the done handler does not close the stream: {done}"
        );
        assert!(
            done.find("stop();") < done.find("JSON.parse"),
            "the stream must be closed before the payload is parsed: {done}"
        );
        // Set through `innerHTML`, which htmx does not watch, so it must be told.
        assert!(
            done.contains("htmx.process(result)"),
            "the done handler no longer hands the answer to htmx: {done}"
        );

        let error = js
            .split_once("addEventListener('error'")
            .expect("the driver does not handle error")
            .1;
        assert!(
            error[..error.find("});").unwrap()].contains("fail("),
            "the error handler does not reach the failure path: {error}"
        );
        let fail = js
            .split_once("function fail(message) {")
            .expect("the driver has no fail()")
            .1;
        assert!(
            fail[..fail.find("\n    }").unwrap()].contains("stop();"),
            "fail() no longer closes the stream, so the browser will reconnect: {fail}"
        );
    }

    #[test]
    fn the_place_switch_reads_a_kept_geohash_before_it_asks_for_a_new_one() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();

        let prime = js
            .split_once("function primePlace() {")
            .expect("app.js has no primePlace()")
            .1;
        let prime = &prime[..prime.find("\n  }").expect("primePlace() does not end")];
        let read = prime
            .find("getItem('engram:place_hash')")
            .expect("primePlace() never reads the kept geohash, so no bundle can carry one");
        let ask = prime
            .find("getCurrentPosition")
            .expect("primePlace() no longer asks for a position");
        assert!(
            read < ask,
            "the kept geohash must be read before the request, not in its callback: {prime}"
        );
        assert!(
            prime.contains("setItem('engram:place_hash'"),
            "a fix that arrives is not kept, so the next load has nothing to read: {prime}"
        );

        let switch = js
            .split_once("function placeSwitch() {")
            .expect("app.js has no placeSwitch()")
            .1;
        assert!(
            switch[..switch.find("\n  }").unwrap()].contains("forgetPlace()"),
            "turning the switch off leaves the kept geohash on disk: {switch}"
        );
        let forget = js
            .split_once("function forgetPlace() {")
            .expect("app.js has no forgetPlace()")
            .1;
        assert!(
            forget[..forget.find("\n  }").unwrap()].contains("removeItem('engram:place_hash')"),
            "forgetPlace() does not remove the kept geohash: {forget}"
        );
    }

    #[test]
    fn no_trigger_scopes_itself_to_a_selector_htmx_will_cut_in_half() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/web/templates");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).expect("the template directory is there") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let html = std::fs::read_to_string(&path).unwrap();
            // Only inside an attribute: comments above these triggers quote the broken form.
            for attr in html.split("hx-trigger=\"").skip(1) {
                let attr = &attr[..attr.find('"').unwrap_or(attr.len())];
                for spec in attr.split(',') {
                    let Some(rest) = spec.split_once("from:") else {
                        continue;
                    };
                    checked += 1;
                    let selector = rest.1.trim();
                    // The one shape htmx reads a second word for.
                    let two_word = ["closest ", "find ", "next ", "previous "]
                        .iter()
                        .any(|p| selector.starts_with(p));
                    assert!(
                        two_word || !selector.contains(char::is_whitespace),
                        "{}: `from:{selector}` binds to `{}` and htmx drops the rest — \
                         give the element an id and name it in one word",
                        path.display(),
                        selector.split_whitespace().next().unwrap_or("")
                    );
                }
            }
        }
        assert!(checked >= 3, "no `from:` triggers were found to check");
    }

    #[tokio::test]
    async fn the_ask_page_carries_every_region_the_stream_driver_looks_up() {
        let js = crate::web::assets::Assets::get("app.js").expect("app.js is embedded");
        let js = String::from_utf8(js.data.into_owned()).unwrap();
        let wanted = pulled(&js, "getElementById('ask-", '\'');
        assert!(
            wanted.len() >= 4,
            "the driver looks up almost nothing, so this test checks almost nothing: {wanted:?}"
        );

        let (app, cookie) = app_holding_something().await;
        let page = get_body(&app, &cookie, "/ui/ask").await;
        for id in wanted {
            assert!(
                page.contains(&format!(r#"id="ask-{id}""#)),
                "app.js drives #ask-{id} and the page has no such element: {page}"
            );
        }
        let form = page
            .split(r#"<form id="box-form""#)
            .nth(1)
            .and_then(|f| f.split("</form>").next())
            .expect("the workspace has a box form");
        assert!(
            !form.contains("hx-post"),
            "the box form still posts through htmx: {form}"
        );
    }

    #[tokio::test]
    async fn the_citations_event_precedes_the_first_token() {
        let (app, cookie, _core) = app_session_and_core_with_an_embedded_base().await;
        let body = ask_over_sse(&app, &cookie, "what+is+alpha").await;
        let cites = body.find("event: citations").expect(&body);
        let token = body.find("event: token").expect(&body);
        assert!(
            cites < token,
            "citations must precede the first token: {body}"
        );
    }

    #[tokio::test]
    async fn an_ask_abandoned_mid_answer_is_not_recorded() {
        let (app, cookie, core) = app_session_and_core_with_an_embedded_base().await;
        let id = post_ask(&app, &cookie, "what+is+alpha").await;
        let res = get_stream(&app, &cookie, &id).await;
        assert_eq!(res.status(), StatusCode::OK);

        // Into the token loop: stopping at `citations` would prove nothing about the completion.
        let seen = read_until(res, "event: token").await;
        assert!(seen.contains("event: retrieved"), "{seen}");
        assert!(seen.contains("event: citations"), "{seen}");
        // `read_until` also stops at end of stream, so an errored ask would pass for the wrong reason.
        assert!(
            seen.contains("event: token"),
            "the answer never started: {seen}"
        );
        assert!(
            !seen.contains("event: done"),
            "the reader has to leave before done for this to be about anything: {seen}"
        );

        assert_eq!(
            core.store.ask_stats().await.unwrap().asked,
            0,
            "an unjudgeable row was written for a reader who left"
        );
    }

    async fn read_until(res: Response, marker: &str) -> String {
        use tokio_stream::StreamExt as _;
        let mut frames = res.into_body().into_data_stream();
        let mut seen = String::new();
        while let Some(chunk) = frames.next().await {
            seen.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            if seen.contains(marker) {
                break;
            }
        }
        // The generator is suspended at the frame just read and never polled again.
        drop(frames);
        seen
    }

    #[tokio::test]
    async fn an_empty_question_is_refused_without_being_parked() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let st = ask_state_over(core).await;
        let (app, cookie) = app_over(&st).await;
        let res = app
            .oneshot(form("/ui/ask", &cookie, "q=+++"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            st.ask_handoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "a question nobody can answer took a slot in the map"
        );
    }

    #[tokio::test]
    async fn parking_a_question_sweeps_out_one_that_expired() {
        let st = ask_state().await;
        let stale = st.ask_handoff_park(a_question(), "me");
        age_out(&st, &stale);
        assert_eq!(
            st.ask_handoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );

        let fresh = st.ask_handoff_park(a_question(), "me");
        let held = st
            .ask_handoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(held.len(), 1, "the expired entry survived the sweep");
        assert!(held.contains_key(&fresh), "the sweep took the live entry");
    }

    #[tokio::test]
    async fn an_expired_handoff_id_is_refused_and_taken_out_of_the_map() {
        let st = ask_state().await;
        let id = st.ask_handoff_park(a_question(), "me");
        age_out(&st, &id);
        assert!(
            st.ask_handoff_take(&id, "me").is_none(),
            "an expired id was honoured"
        );
        assert!(
            st.ask_handoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "a refused id must not stay in the map"
        );
    }

    #[tokio::test]
    async fn a_parked_question_is_spent_only_by_the_subject_who_parked_it() {
        let st = ask_state().await;
        let id = st.ask_handoff_park(a_question(), "alice");
        assert!(
            st.ask_handoff_take(&id, "bob").is_none(),
            "somebody else's question was handed over"
        );
        assert!(
            st.ask_handoff_take(&id, "alice").is_some(),
            "a stranger's attempt spent the asker's own stream"
        );
    }

    fn a_question() -> crate::core::ask::AskRequest {
        crate::core::ask::AskRequest {
            q: "what is alpha".into(),
            limit: None,
            tags: vec![],
            category: None,
        }
    }

    fn age_out(st: &AppState, id: &str) {
        let mut m = st
            .ask_handoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let p = m.get_mut(id).expect("parked");
        p.at -= crate::web::state::ASK_HANDOFF_TTL * 2;
    }

    async fn ask_state() -> AppState {
        ask_state_over(crate::core::test_support::test_core().await).await
    }

    async fn ask_state_over(core: crate::core::Core) -> AppState {
        crate::web::test_support::state_over(core, crate::config::AuthMode::Local).await
    }

    async fn app_over(st: &AppState) -> (axum::Router, String) {
        let cookie = crate::web::test_support::session_cookie(
            st.tenants.control(),
            &st.auth.deployment,
            "user-1",
        )
        .await;
        (crate::web::router(st.clone()), cookie)
    }

    #[test]
    fn marking_an_unsupported_literal_still_reaches_inside_a_code_span() {
        let out = crate::core::ask::check::mark_unsupported(
            "<pre><code>wipefs --all</code></pre>",
            &["wipefs --all".to_string()],
        );
        assert!(
            out.contains(r#"<mark class="unsupported">wipefs --all</mark>"#),
            "{out}"
        );
    }

    #[tokio::test]
    async fn without_an_ask_model_there_is_no_ask_page_and_no_ask_link() {
        let mut core = crate::core::test_support::test_core().await;
        core.completer = None;
        let (app, cookie) = app_with_cookie(core).await;
        let page = get_body(&app, &cookie, "/ui/search").await;
        assert!(!page.contains("href=\"/ui/ask\""), "{page}");
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/ask")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .oneshot(form("/ui/ask", &cookie, "q=anything"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn with_an_ask_model_the_verb_is_there() {
        let (app, cookie, core) = app_session_and_core().await;
        // An empty base hides Ask regardless of configuration.
        core.ingest_capture(crate::core::ingest::Capture::new(
            "LevelDB tombstones survive compaction longer than the manual admits.",
            "ui",
        ))
        .await
        .unwrap();
        let page = get_body(&app, &cookie, "/ui").await;
        assert!(page.contains(r#"data-verb="ask""#), "{page}");
    }

    #[tokio::test]
    async fn the_pursuit_section_is_not_there_when_pursuits_are_off() {
        let (app, cookie) = app_with_session().await;
        let ops = get_body(&app, &cookie, "/ui/insights").await;
        assert!(!ops.contains("<h3>Pursuits</h3>"), "{ops}");
    }

    async fn newest_event(handle: &crate::core::Core) -> String {
        sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM search_events ORDER BY created_at DESC, id DESC LIMIT 1"#
        )
        .fetch_one(&handle.store.pool)
        .await
        .expect("the search the rail was filled by")
    }

    #[tokio::test]
    async fn a_search_while_a_capture_is_read_offers_no_gap() {
        let (app, cookie, handle) = app_session_and_core_with_feedback().await;
        crate::web::test_support::hold_something(&handle).await;
        let rail = get_body(&app, &cookie, "/ui/search/results?q=nothing+here").await;
        assert!(rail.contains("still reading"), "{rail}");
        assert!(!rail.contains("Nothing here has it"), "{rail}");
    }

    #[test]
    fn a_loose_list_while_a_capture_is_read_offers_no_gap() {
        let rail = |reading: bool| {
            askama::Template::render(&ResultsTemplate {
                results: vec![RenderedResult {
                    weak: true,
                    snippet: "a loose one".into(),
                    ..Default::default()
                }],
                all_weak: true,
                event_id: Some("ev-1".into()),
                reading,
                ..Default::default()
            })
            .unwrap()
        };
        assert!(rail(false).contains("Nothing here has it"));
        assert!(
            !rail(true).contains("Nothing here has it"),
            "{}",
            rail(true)
        );
    }

    fn hit_disagreeing() -> crate::core::search::SearchResult {
        crate::core::search::SearchResult {
            artifact_id: "a1".into(),
            text: "Retention is 30 days.".into(),
            disagrees_with: vec![crate::store::pairs::Disagreement {
                artifact_id: "a1".into(),
                created_at: 1_767_139_200,
                other_id: "b2".into(),
                other_title: Some("Backup policy".into()),
                other_created_at: 1_767_225_600,
                detail: Some("30 days there, 14 here".into()),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_row_names_the_note_that_disagrees_with_it() {
        let hit = hit_disagreeing();
        let row = render_hit(0, hit, &Default::default(), false);
        let html = askama::Template::render(&ResultsTemplate {
            results: vec![row],
            ..Default::default()
        })
        .unwrap();
        assert!(html.contains("Disagrees with"), "{html}");
        assert!(html.contains(r#"href="/ui/artifacts/b2""#), "{html}");
        assert!(html.contains("Backup policy"), "{html}");
        assert!(html.contains("(2026-01-01)"), "{html}");
        assert!(html.contains("30 days there, 14 here"), "{html}");

        let mut recalled = hit_disagreeing();
        recalled.via = Some("x".into());
        let html = askama::Template::render(&ResultsTemplate {
            associated: vec![render_hit(0, recalled, &Default::default(), false)],
            ..Default::default()
        })
        .unwrap();
        let below = html
            .split("Recalled by association")
            .nth(1)
            .expect("the associated block was not drawn");
        assert!(below.contains("Disagrees with"), "{html}");
        assert!(below.contains(r#"href="/ui/artifacts/b2""#), "{html}");
    }

    #[tokio::test]
    async fn an_empty_base_records_the_search_and_offers_no_gap() {
        let (app, cookie, handle) = app_session_and_core_with_feedback().await;
        let rail = get_body(&app, &cookie, "/ui/search/results?q=nothing+here").await;
        assert!(rail.contains("No matches."), "{rail}");
        assert!(!rail.contains("Nothing here has it"), "{rail}");
        let event = newest_event(&handle).await;
        assert!(
            rail.contains(&format!(r#"value="{event}""#)),
            "the box still holds the search, so the first capture can answer it: {rail}"
        );
    }

    #[tokio::test]
    async fn the_rail_offers_a_gap_where_nothing_matches() {
        // Over a held, fully read base: both are conditions of the button (`ResultsTemplate::gap`).
        let (app, cookie, handle) = app_session_and_core_with_feedback().await;
        crate::web::test_support::hold_something(&handle).await;
        while let Some(j) = handle.store.claim_job().await.unwrap() {
            handle.store.complete_job(j.id).await.unwrap();
        }
        let rail = get_body(&app, &cookie, "/ui/search/results?q=nothing+here").await;
        assert!(rail.contains("No matches."), "{rail}");
        assert!(rail.contains("Nothing here has it"), "{rail}");
        // The query never reaches the client as data, so a `"` cannot break `hx-vals`.
        let event = newest_event(&handle).await;
        assert!(
            rail.contains(&format!(
                r#"hx-post="/ui/search/{event}/gap?q=nothing%20here""#
            )),
            "{rail}"
        );
        assert!(!rail.contains("hx-vals='{\"q\""), "{rail}");

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/search/{event}/gap?q=nothing%20here"),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let s = handle.store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.gaps, s.pending), (1, 0), "{s:?}");
        assert!(
            body_of(res).await.contains("Recorded as a gap"),
            "the button did not say what it did"
        );
    }

    #[tokio::test]
    async fn an_answer_that_searched_nothing_forgets_the_search_the_box_was_folding_into() {
        let (app, cookie, _handle) = app_session_and_core_with_feedback().await;
        get_body(&app, &cookie, "/ui/search/results?q=fat32").await;
        let idle = get_body(&app, &cookie, "/ui/search/results?q=").await;
        assert!(
            idle.contains(r#"<span hx-swap-oob="innerHTML:#fold-of"></span>"#),
            "{idle}"
        );
    }

    #[tokio::test]
    async fn the_rail_hands_the_box_the_search_it_should_fold_into() {
        let (app, cookie, handle) = app_session_and_core_with_feedback().await;
        let rail = get_body(&app, &cookie, "/ui/search/results?q=fat32").await;
        let first = newest_event(&handle).await;
        assert!(
            rail.contains(&format!(
                r#"<span hx-swap-oob="innerHTML:#fold-of"><input type="hidden" name="fold" value="{first}">"#
            )),
            "{rail}"
        );

        get_body(
            &app,
            &cookie,
            &format!("/ui/search/results?q=fat32+mount&fold={first}"),
        )
        .await;
        assert_eq!(newest_event(&handle).await, first);
        assert_eq!(
            handle.store.feedback_stats(0.0).await.unwrap().captured,
            1,
            "the burst folded into the event the page was holding"
        );

        get_body(&app, &cookie, "/ui/search/results?q=ntfs").await;
        assert_eq!(
            handle.store.feedback_stats(0.0).await.unwrap().captured,
            2,
            "the other window started its own search"
        );
    }

    #[tokio::test]
    async fn a_quotation_mark_in_the_query_does_not_break_the_gap_button() {
        let (app, cookie, handle) = app_session_and_core_with_feedback().await;
        crate::web::test_support::hold_something(&handle).await;
        while let Some(j) = handle.store.claim_job().await.unwrap() {
            handle.store.complete_job(j.id).await.unwrap();
        }
        let rail = get_body(
            &app,
            &cookie,
            "/ui/search/results?q=say%20%22hi%22%20%5Cnow",
        )
        .await;
        let event = newest_event(&handle).await;
        let q = "say%20%22hi%22%20%5Cnow";
        assert!(
            rail.contains(&format!(r#"hx-post="/ui/search/{event}/gap?q={q}""#)),
            "{rail}"
        );
        let res = app
            .clone()
            .oneshot(form(&format!("/ui/search/{event}/gap?q={q}"), &cookie, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(handle.store.feedback_stats(0.0).await.unwrap().gaps, 1);
    }

    #[tokio::test]
    async fn a_stale_bar_says_so_rather_than_writing_over_a_verdict() {
        let (app, cookie, handle, a, event) = searched_app().await;
        handle
            .store
            .judge(
                &event,
                crate::store::feedback::Verdict::Gap,
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();

        for body in [
            format!("verdict=hit&artifact_id={a}"),
            format!("verdict=none&artifact_id={a}"),
        ] {
            let res = app
                .clone()
                .oneshot(form(&format!("/ui/search/{event}/verdict"), &cookie, &body))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            assert!(
                body_of(res).await.contains("already judged"),
                "the bar wrote over the verdict instead of saying it could not"
            );
        }
        let s = handle.store.feedback_stats(0.0).await.unwrap();
        assert_eq!((s.gaps, s.hits, s.judged), (1, 0, 1), "{s:?}");
    }

    #[tokio::test]
    #[ignore = "needs node and a headless Chrome; see `test_support::measure`"]
    async fn a_result_wearing_every_badge_still_shows_its_title() {
        use askama::Template as _;

        let (app, cookie, _core) = app_session_and_core().await;
        let page = get_body(&app, &cookie, "/ui").await;

        let mut row = ranked(true);
        row.primed = true;
        row.due_in = Some("in under an hour".into());
        row.title = "Neues Einlesen von Corpora in engram".into();
        row.snippet = "Um die Corpora in engram neu einzulesen, öffne das Interface.".into();
        let rail = ResultsTemplate {
            results: vec![row],
            terms: "engram".into(),
            ..Default::default()
        }
        .render()
        .expect("the rail fragment");

        // `workspace.html` ships the rail hidden until `hideIdle` runs, and hidden elements measure as nothing.
        let page = page
            .replace(
                r#"<div id="results" role="listbox" aria-label="Results">"#,
                &format!(r#"<div id="results" role="listbox" aria-label="Results">{rail}"#),
            )
            .replace(
                r#"<div id="rail" class="region-rail rail" hidden>"#,
                r#"<div id="rail" class="region-rail rail">"#,
            )
            .replace(
                r#"<div id="pane" class="region-focus pane" hidden"#,
                r#"<div id="pane" class="region-focus pane""#,
            )
            .replace(r#"<div id="idle">"#, r#"<div id="idle" hidden>"#)
            .replace(
                r#"class="regions regions-rail-focus-source"#,
                r#"style="--rail-w:15rem" class="pane-open regions regions-rail-focus-source"#,
            );
        for expected in ["rail-title", "--rail-w", r#"class="region-rail rail">"#] {
            assert!(
                page.contains(expected),
                "the page was not assembled: {expected}"
            );
        }
        assert!(
            !page.contains(r#"class="region-rail rail" hidden"#),
            "the rail is still hidden, so there is nothing to measure"
        );

        let run = crate::web::test_support::measure(&[("results".into(), page)], "1500");
        crate::web::test_support::nothing_is_broken(&run, "1500");
    }
}
