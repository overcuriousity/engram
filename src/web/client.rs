//! The doors the phone presses that the read contract in `docs/api.md` did
//! not list: the decisions and the pages the web interface has beyond reading.
//!
//! Every handler here is the JSON face of a `/ui` route, calling the same
//! store method with the same guards, so a verdict from a phone and one from a
//! browser are one kind of verdict. Where the web handler's logic was worth
//! sharing it was lifted into a `pub(crate)` function beside it and both call
//! that; where it was three lines, it is three lines here too.

use crate::error::{Error, Result};
use crate::tenants::Tenant;
use crate::web::state::AppState;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/search/{id}/verdict", post(search_verdict))
        .route("/search/{id}/gap", post(search_gap))
        .route("/asks/{id}/verdict", post(ask_verdict))
        .route("/asks/{id}/carried", post(ask_carried))
        .route("/asks/{id}/keep", post(ask_keep))
        .route("/moments/{id}/date", post(moment_date))
        .route("/moments/{id}/not-a-reminder", post(not_a_reminder))
        .route("/artifacts/{id}/is-a-reminder", post(is_a_reminder))
        .route("/artifacts/{id}/reviewed", post(artifact_reviewed))
        .route("/artifacts/{id}/links/{other}/dismiss", post(dismiss_link))
        .route("/artifacts/{id}/dwell", post(artifact_dwell))
        .route("/artifacts/{id}/about", get(artifact_about))
        .route("/corpora/{id}/bands", get(corpus_bands))
        .route("/corpora/{id}/reread", post(corpus_reread))
        .route("/corpora/{id}/entry", post(corpus_entry))
        .route("/corpora/{id}/segments/{idx}/unpromote", post(unpromote))
        .route("/days/{date}/entry", post(day_entry))
        .route("/facets", get(facets))
        .route("/echo", get(echo))
        .route("/feedback", get(feedback).delete(purge_feedback))
        .route("/settings/lang", get(get_lang).put(put_lang))
        .route("/settings/notify", get(get_notify).put(put_notify))
        .route("/settings/notify/test", post(test_notify))
        .route("/insights/machine", get(machine))
        .route("/insights/report", get(report))
}

// ── Search verdicts ──────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct SearchVerdictBody {
    /// `hit`, `no`, `skip`, or `none` to take a verdict back.
    pub verdict: String,
    pub artifact_id: String,
}

/// What the bar shows afterwards. `already` is the one refusal that is not
/// an error: another door judged this search first, and the bar says so in
/// words rather than failing.
#[derive(serde::Serialize)]
pub struct VerdictAnswer {
    /// `hit`, `no`, `skip`, or empty for no verdict.
    pub state: &'static str,
    pub already: bool,
}

/// `POST /search/{id}/verdict`: *was this what you were looking for?*, from
/// the phone. The rules are `workspace::judge_search`'s, and only that
/// function's.
async fn search_verdict(
    tenant: Tenant,
    Path(id): Path<String>,
    Json(b): Json<SearchVerdictBody>,
) -> Result<Json<VerdictAnswer>> {
    let out = crate::web::workspace::judge_search(&tenant, &id, &b.verdict, &b.artifact_id).await?;
    Ok(Json(match out {
        Some(state) => VerdictAnswer {
            state,
            already: false,
        },
        None => VerdictAnswer {
            state: "",
            already: true,
        },
    }))
}

#[derive(serde::Deserialize)]
pub struct GapBody {
    pub q: String,
}

#[derive(serde::Serialize)]
pub struct GapAnswer {
    pub recorded: bool,
}

/// `POST /search/{id}/gap`: *nothing here has it*, against the search that
/// filled the list. `recorded: false` means the search was already judged.
async fn search_gap(
    tenant: Tenant,
    Path(id): Path<String>,
    Json(b): Json<GapBody>,
) -> Result<Json<GapAnswer>> {
    let recorded = crate::web::workspace::gap_search(&tenant, &id, &b.q).await?;
    Ok(Json(GapAnswer { recorded }))
}

// ── Ask verdicts, carried, keep ──────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct AskVerdictBody {
    /// `right`, `wrong`, `nothing_here`, or `none` to take it back.
    pub verdict: String,
}

#[derive(serde::Serialize)]
pub struct AskVerdictAnswer {
    /// The verdict as the web's bar words it — `right`, `wrong`, `nothing
    /// here` — or null where the question stands unjudged.
    pub verdict: Option<String>,
}

async fn ask_verdict(
    tenant: Tenant,
    Path(id): Path<String>,
    Json(b): Json<AskVerdictBody>,
) -> Result<Json<AskVerdictAnswer>> {
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    match b.verdict.as_str() {
        "none" => tenant.core.store.unjudge_ask(&id).await?,
        v => {
            let verdict = crate::store::asks::AskVerdict::parse(v)
                .ok_or_else(|| Error::Validation(format!("unknown verdict {v}")))?;
            tenant.core.store.judge_ask(&id, verdict).await?;
        }
    }
    let ev = tenant
        .core
        .store
        .ask_event(&id)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(AskVerdictAnswer {
        verdict: ev.verdict.map(crate::web::workspace::verdict_label),
    }))
}

#[derive(serde::Deserialize)]
pub struct CarriedBody {
    /// The excerpt's number, 1-based, as the answer cites it.
    pub n: i64,
}

#[derive(serde::Serialize)]
pub struct CarriedAnswer {
    pub carried: bool,
    /// Saying an excerpt carried the answer is saying the answer was right,
    /// so the bar's state comes back with the toggle, as it does on the web.
    pub verdict: Option<String>,
}

async fn ask_carried(
    tenant: Tenant,
    Path(id): Path<String>,
    Json(b): Json<CarriedBody>,
) -> Result<Json<CarriedAnswer>> {
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    let carried = tenant.core.store.toggle_carried(&id, b.n).await?;
    let ev = tenant
        .core
        .store
        .ask_event(&id)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(CarriedAnswer {
        carried,
        verdict: ev.verdict.map(crate::web::workspace::verdict_label),
    }))
}

/// What became of a kept answer — the words `_ask_kept.html` says.
#[derive(serde::Serialize)]
pub struct KeptAnswer {
    /// The source it became.
    pub id: String,
    pub duplicate: bool,
    /// Stored, but waiting on a decision between it and a near-identical source.
    pub parked: bool,
    pub near_dupe_percent: i64,
}

/// `POST /asks/{id}/keep`: store the answer as a source, with the question
/// and the artifacts it was written from. `workspace::ask_keep`'s rules.
async fn ask_keep(tenant: Tenant, Path(id): Path<String>) -> Result<Json<KeptAnswer>> {
    if !tenant.core.asks() {
        return Err(Error::NotFound);
    }
    let ev = tenant
        .core
        .store
        .ask_event(&id)
        .await?
        .ok_or(Error::NotFound)?;
    let out = tenant
        .core
        .ingest_capture(
            crate::core::ingest::Capture::new(&ev.answer, crate::core::ingest::ORIGIN_ASK)
                .with_ask(&ev.id, &ev.question, &ev.citations),
        )
        .await?;
    Ok(Json(KeptAnswer {
        id: out.id,
        duplicate: out.duplicate,
        parked: out.near_duplicate.is_some(),
        near_dupe_percent: out
            .near_duplicate
            .as_ref()
            .map(|n| (n.similarity * 100.0).round() as i64)
            .unwrap_or(0),
    }))
}

// ── Reminders ────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct DateBody {
    /// The new instant, Unix seconds.
    pub at: i64,
    /// The zone it was chosen in.
    pub tz: String,
}

/// `POST /moments/{id}/date`: move a reminder, or give an undated one a date.
async fn moment_date(
    tenant: Tenant,
    Path(id): Path<String>,
    Json(b): Json<DateBody>,
) -> Result<StatusCode> {
    tenant
        .core
        .store
        .moment(&id)
        .await?
        .ok_or(Error::NotFound)?;
    let tz = crate::core::moments::zone(Some(&b.tz));
    tenant.core.store.move_moment(&id, b.at, tz.name()).await?;
    tenant.core.store.rearm_remind().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Serialize)]
pub struct NotAReminderAnswer {
    /// The artifact `POST /artifacts/{id}/is-a-reminder` would restore the
    /// reminder on, where that is possible; null where the read cannot be
    /// handed back.
    pub undo: Option<String>,
}

/// `POST /moments/{id}/not-a-reminder`: *I never asked to be reminded of
/// this*. The stage's reading is retracted; a reminder somebody set is left.
async fn not_a_reminder(
    tenant: Tenant,
    Path(id): Path<String>,
) -> Result<Json<NotAReminderAnswer>> {
    let m = tenant
        .core
        .store
        .moment(&id)
        .await?
        .ok_or(Error::NotFound)?;
    let takes_it_back = tenant
        .core
        .can_be_a_reminder(&m.artifact_id)
        .await
        .unwrap_or(false);
    let did = tenant.core.set_reminder(&m.artifact_id, false).await?;
    tenant
        .core
        .store
        .undo_action_on(
            &id,
            crate::store::actions::Kind::Moment,
            crate::store::actions::UndoneBy::Operator,
            "not a reminder",
        )
        .await?;
    Ok(Json(NotAReminderAnswer {
        undo: (did && takes_it_back).then_some(m.artifact_id),
    }))
}

/// `POST /artifacts/{id}/is-a-reminder`: the undo of the above.
async fn is_a_reminder(tenant: Tenant, Path(id): Path<String>) -> Result<StatusCode> {
    tenant.core.set_reminder(&id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Artifacts ────────────────────────────────────────────────────────────

/// `POST /artifacts/{id}/reviewed`: clear the verification flags. A
/// judgement, not a fix — `artifact::mark_artifact_reviewed`'s rule.
async fn artifact_reviewed(tenant: Tenant, Path(id): Path<String>) -> Result<StatusCode> {
    let c = tenant.core.store.get_artifact(&id).await?;
    if c.flags.iter().any(|f| f == "orphaned_source") {
        tenant.core.store.accept_source_loss(&id).await?;
    }
    tenant.core.store.clear_artifact_flags(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /artifacts/{id}/links/{other}/dismiss`: *not related*. Final for
/// that pair.
async fn dismiss_link(
    tenant: Tenant,
    Path((id, other)): Path<(String, String)>,
) -> Result<StatusCode> {
    tenant.core.store.dismiss_link(&id, &other).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct DwellBody {
    pub secs: i64,
}

/// `POST /artifacts/{id}/dwell`: how long an artifact was on screen. Scoped
/// to the person, as the web's is.
async fn artifact_dwell(
    tenant: Tenant,
    Path(id): Path<String>,
    Json(b): Json<DwellBody>,
) -> Result<StatusCode> {
    tenant
        .core
        .record_dwell(&id, b.secs, Some(&tenant.user.subject));
    Ok(StatusCode::NO_CONTENT)
}

/// The lines of the pane that are about the artifact rather than of it.
#[derive(serde::Serialize)]
pub struct About {
    /// What the base found when this arrived, as a sentence. Null before the
    /// artifact has been integrated.
    pub tag: Option<String>,
    /// The probes for this artifact, one line each.
    pub probes: Vec<String>,
    /// The open condensation's action id, where the live text is a condensed
    /// one: `POST /condensations/{id}/undo` puts the last version back.
    pub condensed: Option<String>,
    /// `in 2 h`, `3 d ago`, … when this artifact carries an open reminder.
    pub due_in: Option<String>,
}

/// `GET /artifacts/{id}/about`. Its own read, like `related`: none of this is
/// the artifact, and none of it should hold the text up.
async fn artifact_about(tenant: Tenant, Path(id): Path<String>) -> Result<Json<About>> {
    let c = tenant.core.store.get_artifact(&id).await?;
    let a = crate::web::artifact::about(&tenant.core, &c).await?;
    Ok(Json(About {
        tag: a.tag,
        probes: a.probes,
        condensed: a.condensed,
        due_in: a.due_in,
    }))
}

// ── The corpus page ──────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct BandOut {
    pub from: i64,
    pub to: i64,
    /// Nothing was written from these lines, and the loss is final.
    pub gap: bool,
    /// The window a re-read would read, where one is offered: `reads lines
    /// 118–141`.
    pub reread: Option<String>,
    pub lines: Vec<crate::web::api::SourceLine>,
    /// The artifacts written from these lines, in order. The first band an
    /// artifact appears in carries it; a later band it also spans names it in
    /// `echoes`.
    pub artifact_ids: Vec<String>,
    pub echoes: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct PromotedOut {
    pub idx: i64,
    pub from: i64,
    pub to: i64,
}

/// `corpus.html` as data: what stands above the bands, and the bands.
#[derive(serde::Serialize)]
pub struct CorpusPage {
    pub image: bool,
    pub pdf: bool,
    /// A photo or a PDF nothing has read yet.
    pub unread: bool,
    pub restored: bool,
    pub note: Option<String>,
    pub coverage: Option<String>,
    pub meta: Vec<(String, String)>,
    pub exif: Vec<(String, String)>,
    pub promoted: Vec<PromotedOut>,
    pub bands: Vec<BandOut>,
    /// Artifacts that name no lines of this capture.
    pub unplaced: Vec<String>,
    /// Model-written artifacts with a source here.
    pub written_from: Vec<String>,
}

/// `GET /corpora/{id}/bands`: the source band by band beside what was
/// written from it. The same cut `corpus_detail` renders, from the same
/// function, so a phone and a browser show the same red.
async fn corpus_bands(tenant: Tenant, Path(cid): Path<String>) -> Result<Json<CorpusPage>> {
    use crate::store::corpora::CorpusStatus;
    let s = tenant.core.store.get_corpus(&cid).await?;
    let chunks = tenant.core.store.artifacts_for_corpus(&cid).await?;
    let restored = s.restored_at.is_some();
    let spans: Vec<(String, crate::store::artifacts::CorpusSpan)> = if restored {
        Vec::new()
    } else {
        chunks
            .iter()
            .filter_map(|c| c.corpus_span.clone().map(|sp| (c.id.clone(), sp)))
            .collect()
    };
    let unplaced: Vec<String> = chunks
        .iter()
        .filter(|c| restored || c.corpus_span.is_none())
        .map(|c| c.id.clone())
        .collect();
    let segments = tenant.core.store.segments_for_corpus(&cid).await?;
    let losses_are_final = crate::web::corpus::coverage_final(&s.status)
        && !segments.is_empty()
        && unplaced.is_empty();
    let mut carded: std::collections::HashSet<String> = std::collections::HashSet::new();
    let bands: Vec<BandOut> = if restored {
        Vec::new()
    } else {
        crate::web::corpus_view::bands(&s.raw_text, &spans, None)
            .into_iter()
            .map(|b| {
                let (mut artifact_ids, mut echoes) = (Vec::new(), Vec::new());
                for id in &b.artifact_ids {
                    if carded.insert(id.clone()) {
                        artifact_ids.push(id.clone());
                    } else {
                        echoes.push(id.clone());
                    }
                }
                BandOut {
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
                    lines: b
                        .lines
                        .into_iter()
                        .map(|l| crate::web::api::SourceLine {
                            number: l.number,
                            text: l.text,
                            in_span: l.in_span,
                        })
                        .collect(),
                    artifact_ids,
                    echoes,
                }
            })
            .collect()
    };
    let coverage = if chunks
        .iter()
        .all(|c| c.provenance == crate::store::artifacts::Provenance::Passage)
    {
        None
    } else {
        s.coverage.map(|c| format!("{:.0}%", c * 100.0))
    };
    let image = s.origin == crate::core::ingest::ORIGIN_IMAGE;
    let pdf = s.origin == crate::core::ingest::ORIGIN_PDF;
    let unread = (image && (s.status == CorpusStatus::Describing || s.raw_text.trim().is_empty()))
        || (pdf && (s.status == CorpusStatus::Extracting || s.raw_text.trim().is_empty()));
    let written_from: Vec<String> = tenant
        .core
        .store
        .artifacts_originating_in(&cid)
        .await?
        .iter()
        .filter(|c| c.in_results())
        .map(|c| c.id.clone())
        .collect();
    let promoted = segments
        .iter()
        .filter(|w| w.state == crate::store::segments::SegmentState::Done)
        .filter(|w| {
            chunks.iter().any(|c| {
                c.segment_idx == Some(w.idx)
                    && c.provenance == crate::store::artifacts::Provenance::Captured
            })
        })
        .map(|w| PromotedOut {
            idx: w.idx,
            from: w.start_line,
            to: w.end_line,
        })
        .collect();
    Ok(Json(CorpusPage {
        image,
        pdf,
        unread,
        restored,
        note: s.metadata["note"].as_str().map(str::to_string),
        coverage,
        meta: crate::web::corpus::metadata_rows(&s.metadata),
        exif: crate::web::corpus::exif_tag_rows(&s.metadata),
        promoted,
        bands,
        unplaced,
        written_from,
    }))
}

#[derive(serde::Deserialize)]
pub struct RereadBody {
    pub from: i64,
    pub to: i64,
}

/// `POST /corpora/{id}/reread`: read one passage again. `202` where a read
/// was queued, `204` where there was nothing to re-read — a capture still
/// being read, or a band that is not a final loss.
async fn corpus_reread(
    tenant: Tenant,
    Path(cid): Path<String>,
    Json(b): Json<RereadBody>,
) -> Result<StatusCode> {
    Ok(
        match crate::web::corpus::reread(&tenant, &cid, b.from, b.to).await? {
            true => StatusCode::ACCEPTED,
            false => StatusCode::NO_CONTENT,
        },
    )
}

#[derive(serde::Deserialize)]
pub struct EntryBody {
    pub on: bool,
}

/// `POST /corpora/{id}/entry`: file a capture as the day's entry, or take it
/// back out.
async fn corpus_entry(
    tenant: Tenant,
    Path(cid): Path<String>,
    Json(b): Json<EntryBody>,
) -> Result<StatusCode> {
    tenant.core.set_entry(&cid, b.on).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /corpora/{id}/segments/{idx}/unpromote`: put the verbatim text back
/// in results and retire what a promotion wrote.
async fn unpromote(tenant: Tenant, Path((cid, idx)): Path<(String, i64)>) -> Result<StatusCode> {
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
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct DayEntryBody {
    pub text: String,
    pub tz: String,
}

#[derive(serde::Serialize)]
pub struct DayEntryAnswer {
    pub id: String,
}

/// `POST /days/{date}/entry`: an entry *into this day*, whenever it is
/// written. `day::entry`'s rules: the date is a date, and the zone goes
/// through the zone table.
async fn day_entry(
    tenant: Tenant,
    Path(date): Path<String>,
    headers: axum::http::HeaderMap,
    Json(b): Json<DayEntryBody>,
) -> Result<Json<DayEntryAnswer>> {
    let Ok(day) = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
        return Err(Error::NotFound);
    };
    if b.text.trim().is_empty() {
        return Err(Error::Validation("the entry is empty".into()));
    }
    let date = day.format("%Y-%m-%d").to_string();
    let tz_name = crate::core::moments::zone(Some(&b.tz)).name().to_string();
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
    let mut c = crate::core::ingest::Capture::new(&b.text, crate::core::ingest::ORIGIN_JOURNAL)
        .from_channel(crate::core::ingest::ORIGIN_WEB)
        .with_lang(lang)
        .with_tz(Some(tz_name));
    c.metadata["day"] = serde_json::Value::String(date);
    let out = tenant.core.ingest_capture(c).await?;
    Ok(Json(DayEntryAnswer { id: out.id }))
}

// ── Facets, feedback, settings ───────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct EchoParams {
    #[serde(default)]
    pub q: String,
}

/// What capture will do with the box, said before it is pressed. The same
/// count the web's echo makes — the tokenizer and the budget the fork itself
/// uses, no model call. Empty `kind` for an empty box.
#[derive(serde::Serialize)]
pub struct Echo {
    pub kind: String,
    pub detail: String,
}

async fn echo(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    axum::extract::Query(p): axum::extract::Query<EchoParams>,
) -> Result<Json<Echo>> {
    let lang = crate::web::state::capture_lang(&tenant, &headers).await;
    let t = crate::web::ui::fate_echo(&tenant.core, &p.q, lang);
    Ok(Json(Echo {
        kind: t.kind.to_string(),
        detail: t.detail,
    }))
}

/// `GET /facets`: what the box's chips narrow by.
async fn facets(tenant: Tenant) -> Result<Json<crate::vector::Facets>> {
    Ok(Json(
        tenant
            .core
            .vectors
            .facets(crate::web::ui::FACET_LIMIT)
            .await
            .unwrap_or_default(),
    ))
}

#[derive(serde::Serialize)]
pub struct Recorded {
    pub captured: i64,
    pub pending: i64,
    pub judged: i64,
}

#[derive(serde::Serialize)]
pub struct AskedRecorded {
    pub asked: i64,
    pub judged: i64,
}

/// What this installation is keeping about how it is used. Both null while
/// `[learn]` is off, which is when nothing is recorded.
#[derive(serde::Serialize)]
pub struct Feedback {
    pub searches: Option<Recorded>,
    pub asks: Option<AskedRecorded>,
}

async fn feedback(tenant: Tenant) -> Result<Json<Feedback>> {
    if !tenant.core.learn.enabled {
        return Ok(Json(Feedback {
            searches: None,
            asks: None,
        }));
    }
    let f = tenant
        .core
        .store
        .feedback_stats(tenant.core.weak_below())
        .await?;
    let a = tenant.core.store.ask_stats().await?;
    Ok(Json(Feedback {
        searches: Some(Recorded {
            captured: f.captured,
            pending: f.pending,
            judged: f.judged,
        }),
        asks: Some(AskedRecorded {
            asked: a.asked,
            judged: a.judged,
        }),
    }))
}

#[derive(serde::Serialize)]
pub struct Purged {
    pub dropped: u64,
}

/// `DELETE /feedback`: forget every captured search, question and situation.
/// `settings::purge_feedback_ui`'s two steps, in the same order.
async fn purge_feedback(tenant: Tenant) -> Result<Json<Purged>> {
    let cleared = tenant.core.forget_situations().await;
    let n = tenant.core.store.purge_feedback().await?;
    tracing::info!(
        dropped = n,
        points_cleared = cleared,
        "captured searches, questions and situations deleted by the operator"
    );
    Ok(Json(Purged { dropped: n }))
}

#[derive(serde::Serialize)]
pub struct LangRow {
    pub value: &'static str,
    pub label: &'static str,
}

#[derive(serde::Serialize)]
pub struct LangSetting {
    /// The tag chosen, or empty for automatic.
    pub chosen: String,
    pub langs: Vec<LangRow>,
}

async fn get_lang(tenant: Tenant) -> Result<Json<LangSetting>> {
    let chosen = tenant.core.store.control.lang(&tenant.user.subject).await?;
    Ok(Json(LangSetting {
        chosen: chosen.map(|l| l.tag().to_string()).unwrap_or_default(),
        langs: crate::infer::lang::Lang::ALL
            .iter()
            .map(|l| LangRow {
                value: l.tag(),
                label: l.endonym(),
            })
            .collect(),
    }))
}

#[derive(serde::Deserialize)]
pub struct LangBody {
    #[serde(default)]
    pub lang: String,
}

async fn put_lang(tenant: Tenant, Json(b): Json<LangBody>) -> Result<StatusCode> {
    let chosen = match b.lang.trim() {
        "" => None,
        tag => Some(
            crate::infer::lang::Lang::parse(tag)
                .ok_or_else(|| Error::Validation(format!("lang: `{tag}` is not one of the ten")))?,
        ),
    };
    tenant
        .core
        .store
        .control
        .set_lang(&tenant.user.subject, chosen)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_notify(tenant: Tenant) -> Result<Json<crate::web::settings::NotifySetting>> {
    Ok(Json(crate::web::settings::notify_setting(&tenant).await?))
}

#[derive(serde::Deserialize)]
pub struct NotifyBody {
    #[serde(default)]
    pub gotify_url: String,
    #[serde(default)]
    pub gotify_token: String,
    #[serde(default)]
    pub up_endpoint: String,
}

async fn put_notify(tenant: Tenant, Json(b): Json<NotifyBody>) -> Result<StatusCode> {
    crate::web::settings::set_notify(&tenant, &b.gotify_url, &b.gotify_token, &b.up_endpoint)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct NotifyTestBody {
    pub channel: String,
}

#[derive(serde::Serialize)]
pub struct NotifyTested {
    pub sent: bool,
    /// Why not, in the words the web says.
    pub error: Option<String>,
}

async fn test_notify(tenant: Tenant, Json(b): Json<NotifyTestBody>) -> Result<Json<NotifyTested>> {
    Ok(Json(
        match crate::web::settings::test_channel(&tenant, &b.channel).await? {
            Ok(()) => NotifyTested {
                sent: true,
                error: None,
            },
            Err(why) => NotifyTested {
                sent: false,
                error: Some(why),
            },
        },
    ))
}

// ── Insights ─────────────────────────────────────────────────────────────

/// `GET /insights/machine`: what the machine is doing — the disclosure at the
/// foot of Insights, as data.
async fn machine(tenant: Tenant) -> Result<Json<crate::web::insights::Machine>> {
    Ok(Json(crate::web::insights::machine(&tenant).await?))
}

/// `GET /insights/report`: last night, the ranking, and the pursuits line —
/// the sentences Insights says about what the base did on its own.
async fn report(tenant: Tenant) -> Result<Json<crate::web::insights::Report>> {
    Ok(Json(crate::web::insights::report(&tenant).await?))
}

#[cfg(test)]
mod tests {
    use crate::core::ingest::Capture;
    use crate::web::api::tests::{app_from_core, app_token_and_core, post_json};
    use crate::web::test_support::json_of;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn get(uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("GET")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    fn with_body(method: &str, uri: &str, token: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method(method)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn bare(method: &str, uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method(method)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    /// A learning core with one read capture, and its live artifact.
    async fn learning_base() -> (axum::Router, String, crate::core::Core, String) {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let out = core
            .ingest_capture(Capture::new("Send the invoice to the client", "ui"))
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
        let (app, token, core) = app_from_core(core).await;
        (app, token, core, aid)
    }

    #[tokio::test]
    async fn a_search_from_the_app_is_recorded_and_the_result_it_opens_can_be_judged() {
        let (app, token, _core, aid) = learning_base().await;

        // Without the door, a bearer search is an agent's: nothing to judge.
        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/search?q=invoice", &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(v.get("event").is_none(), "{v}");

        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/search?q=invoice&door=app", &token))
                .await
                .unwrap(),
        )
        .await;
        let ev = v["event"]
            .as_str()
            .expect("the app's search waits for its id")
            .to_string();

        // The open names the search; the answer says which one it was attributed to.
        let d = json_of(
            app.clone()
                .oneshot(get(&format!("/api/v1/artifacts/{aid}?event={ev}"), &token))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(d["search_event"], ev, "{d}");
        // Opened without one, there is no bar to draw.
        let d = json_of(
            app.clone()
                .oneshot(get(&format!("/api/v1/artifacts/{aid}"), &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(d["search_event"].is_null(), "{d}");

        let judge = |verdict: &str| {
            with_body(
                "POST",
                &format!("/api/v1/search/{ev}/verdict"),
                &token,
                serde_json::json!({ "verdict": verdict, "artifact_id": aid }),
            )
        };
        let v = json_of(app.clone().oneshot(judge("hit")).await.unwrap()).await;
        assert_eq!(v["state"], "hit");
        assert_eq!(v["already"], false);
        // Judged twice is the sentence, not an error.
        let v = json_of(app.clone().oneshot(judge("hit")).await.unwrap()).await;
        assert_eq!(v["already"], true, "{v}");
        let v = json_of(app.clone().oneshot(judge("none")).await.unwrap()).await;
        assert_eq!(v["state"], "");
        let v = json_of(app.clone().oneshot(judge("skip")).await.unwrap()).await;
        assert_eq!(v["state"], "skip");
        let res = app.clone().oneshot(judge("maybe")).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // A gap, against a fresh search, and only once.
        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/search?q=quarterly%20tax&door=app", &token))
                .await
                .unwrap(),
        )
        .await;
        let ev2 = v["event"].as_str().unwrap().to_string();
        let gap = || {
            with_body(
                "POST",
                &format!("/api/v1/search/{ev2}/gap"),
                &token,
                serde_json::json!({ "q": "quarterly tax" }),
            )
        };
        let v = json_of(app.clone().oneshot(gap()).await.unwrap()).await;
        assert_eq!(v["recorded"], true, "{v}");
        let v = json_of(app.clone().oneshot(gap()).await.unwrap()).await;
        assert_eq!(v["recorded"], false, "{v}");
    }

    #[tokio::test]
    async fn a_question_from_the_app_is_recorded_judged_and_kept() {
        let (app, token, core, _aid) = learning_base().await;
        let v = json_of(
            app.clone()
                .oneshot(post_json(
                    "/api/v1/ask?door=app",
                    &token,
                    serde_json::json!({ "q": "where does the invoice go" }),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = v["event_id"]
            .as_str()
            .expect("a question from the app is recorded")
            .to_string();

        let verdict = |w: &str| {
            with_body(
                "POST",
                &format!("/api/v1/asks/{id}/verdict"),
                &token,
                serde_json::json!({ "verdict": w }),
            )
        };
        let v = json_of(app.clone().oneshot(verdict("right")).await.unwrap()).await;
        assert_eq!(v["verdict"], "right");
        let v = json_of(app.clone().oneshot(verdict("nothing_here")).await.unwrap()).await;
        assert_eq!(v["verdict"], "nothing here");
        let v = json_of(app.clone().oneshot(verdict("none")).await.unwrap()).await;
        assert!(v["verdict"].is_null(), "{v}");

        let carried = || {
            with_body(
                "POST",
                &format!("/api/v1/asks/{id}/carried"),
                &token,
                serde_json::json!({ "n": 1 }),
            )
        };
        let a = json_of(app.clone().oneshot(carried()).await.unwrap()).await;
        let b = json_of(app.clone().oneshot(carried()).await.unwrap()).await;
        assert!(
            a["carried"].is_boolean() && b["carried"].is_boolean(),
            "{a} {b}"
        );
        assert_ne!(a["carried"], b["carried"], "a toggle toggles");

        let before = core.store.held_brief().await.unwrap().0;
        let v = json_of(
            app.clone()
                .oneshot(bare("POST", &format!("/api/v1/asks/{id}/keep"), &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(v["id"].is_string(), "{v}");
        assert_eq!(v["duplicate"], false);
        assert_eq!(
            core.store.held_brief().await.unwrap().0,
            before + 1,
            "kept as a source"
        );

        // And through the capture door with the question riding along.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/capture?from_ask={id}"))
                    .method("POST")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "text/plain; charset=utf-8")
                    .body(Body::from("my own wording of the answer"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(res.status().is_success(), "{}", res.status());
        let out = json_of(res).await;
        let c = core
            .store
            .get_corpus(out["id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(c.metadata["ask"]["event_id"], id, "{}", c.metadata);

        let res = app
            .oneshot(bare("POST", "/api/v1/asks/no-such/keep", &token))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_reminder_can_be_given_a_date_and_a_set_one_is_never_retracted() {
        use crate::store::moments::{Kind, NewMoment, Source};
        let (app, token, core, aid) = learning_base().await;
        let mid = core
            .store
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: None,
                tz: "UTC".into(),
                rule: None,
                source: Source::Set,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(with_body(
                "POST",
                &format!("/api/v1/moments/{mid}/date"),
                &token,
                serde_json::json!({ "at": 1_900_000_000, "tz": "Europe/Berlin" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let m = core.store.moment(&mid).await.unwrap().unwrap();
        assert_eq!(m.at, Some(1_900_000_000));
        assert_eq!(m.tz, "Europe/Berlin");

        let res = app
            .clone()
            .oneshot(with_body(
                "POST",
                "/api/v1/moments/no-such/date",
                &token,
                serde_json::json!({ "at": 1_900_000_000, "tz": "UTC" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // A reminder somebody set is not a misreading: nothing to retract, no undo offered.
        let v = json_of(
            app.oneshot(bare(
                "POST",
                &format!("/api/v1/moments/{mid}/not-a-reminder"),
                &token,
            ))
            .await
            .unwrap(),
        )
        .await;
        assert!(v["undo"].is_null(), "{v}");
        assert!(core.store.moment(&mid).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn about_reviewed_dismiss_and_dwell() {
        let (app, token, core, aid) = learning_base().await;
        let v = json_of(
            app.clone()
                .oneshot(get(&format!("/api/v1/artifacts/{aid}/about"), &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(v["probes"].is_array(), "{v}");
        assert!(
            v.get("tag").is_some() && v.get("condensed").is_some() && v.get("due_in").is_some(),
            "{v}"
        );

        core.store
            .set_artifact_flags(&aid, &["stale".to_string()], Some("a detail"))
            .await
            .unwrap();
        assert!(
            !core
                .store
                .get_artifact(&aid)
                .await
                .unwrap()
                .flags
                .is_empty()
        );
        let res = app
            .clone()
            .oneshot(bare(
                "POST",
                &format!("/api/v1/artifacts/{aid}/reviewed"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(
            core.store
                .get_artifact(&aid)
                .await
                .unwrap()
                .flags
                .is_empty()
        );

        let res = app
            .clone()
            .oneshot(bare(
                "POST",
                &format!("/api/v1/artifacts/{aid}/links/other/dismiss"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = app
            .oneshot(with_body(
                "POST",
                &format!("/api/v1/artifacts/{aid}/dwell"),
                &token,
                serde_json::json!({ "secs": 12 }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn the_corpus_page_as_data_and_the_entry_switch() {
        let (app, token, core) = app_token_and_core().await;
        let out = core
            .ingest_capture(Capture::new(
                "alpha line\n\nbravo line\n\ncharlie line",
                "ui",
            ))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let id = out.id.clone();
        let v = json_of(
            app.clone()
                .oneshot(get(&format!("/api/v1/corpora/{id}/bands"), &token))
                .await
                .unwrap(),
        )
        .await;
        let bands = v["bands"].as_array().unwrap();
        assert!(!bands.is_empty(), "{v}");
        assert_eq!(bands[0]["lines"][0]["number"], 1, "{v}");
        assert_eq!(v["image"], false);
        assert_eq!(v["restored"], false);
        assert!(v["unplaced"].is_array() && v["written_from"].is_array());

        // A band that is not a loss has nothing to re-read: said, not errored.
        let res = app
            .clone()
            .oneshot(with_body(
                "POST",
                &format!("/api/v1/corpora/{id}/reread"),
                &token,
                serde_json::json!({ "from": 1, "to": 1 }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = app
            .clone()
            .oneshot(with_body(
                "POST",
                &format!("/api/v1/corpora/{id}/entry"),
                &token,
                serde_json::json!({ "on": true }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            core.store.get_corpus(&id).await.unwrap().origin,
            crate::core::ingest::ORIGIN_JOURNAL
        );
        app.oneshot(with_body(
            "POST",
            &format!("/api/v1/corpora/{id}/entry"),
            &token,
            serde_json::json!({ "on": false }),
        ))
        .await
        .unwrap();
        assert_ne!(
            core.store.get_corpus(&id).await.unwrap().origin,
            crate::core::ingest::ORIGIN_JOURNAL
        );
    }

    #[tokio::test]
    async fn an_entry_goes_into_its_day() {
        let (app, token, core) = app_token_and_core().await;
        let v = json_of(
            app.clone()
                .oneshot(with_body(
                    "POST",
                    "/api/v1/days/2026-09-18/entry",
                    &token,
                    serde_json::json!({ "text": "Long day.", "tz": "Europe/Berlin" }),
                ))
                .await
                .unwrap(),
        )
        .await;
        let c = core
            .store
            .get_corpus(v["id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(c.origin, crate::core::ingest::ORIGIN_JOURNAL);
        assert_eq!(c.metadata["day"], "2026-09-18");
        let res = app
            .clone()
            .oneshot(with_body(
                "POST",
                "/api/v1/days/garbage/entry",
                &token,
                serde_json::json!({ "text": "x", "tz": "UTC" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .oneshot(with_body(
                "POST",
                "/api/v1/days/2026-09-18/entry",
                &token,
                serde_json::json!({ "text": "  ", "tz": "UTC" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn the_echo_says_what_a_paste_becomes() {
        let (app, token) = {
            let (app, token, _core) = app_token_and_core().await;
            (app, token)
        };
        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/echo?q=a%20short%20note", &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(v["kind"].is_string() && v["detail"].is_string(), "{v}");
        let v = json_of(app.oneshot(get("/api/v1/echo?q=", &token)).await.unwrap()).await;
        assert_eq!(v["kind"], "", "an empty box says nothing");
    }

    #[tokio::test]
    async fn facets_feedback_language_and_notifications() {
        let (app, token, _core, _aid) = learning_base().await;
        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/facets", &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(v["categories"].is_array(), "{v}");

        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/feedback", &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(v["searches"]["captured"].is_number(), "{v}");
        assert!(v["asks"]["asked"].is_number(), "{v}");
        let v = json_of(
            app.clone()
                .oneshot(bare("DELETE", "/api/v1/feedback", &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(v["dropped"].is_number(), "{v}");

        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/settings/lang", &token))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(v["chosen"], "");
        assert_eq!(v["langs"].as_array().unwrap().len(), 10, "{v}");
        let res = app
            .clone()
            .oneshot(with_body(
                "PUT",
                "/api/v1/settings/lang",
                &token,
                serde_json::json!({ "lang": "de" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/settings/lang", &token))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(v["chosen"], "de");
        let res = app
            .clone()
            .oneshot(with_body(
                "PUT",
                "/api/v1/settings/lang",
                &token,
                serde_json::json!({ "lang": "xx" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/settings/notify", &token))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(v["gotify_token_set"], false, "{v}");
        let res = app
            .clone()
            .oneshot(with_body(
                "PUT",
                "/api/v1/settings/notify",
                &token,
                serde_json::json!({ "gotify_url": "https://gotify.example/message", "gotify_token": "t", "up_endpoint": "" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/settings/notify", &token))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(v["gotify_url"], "https://gotify.example/message");
        assert_eq!(v["gotify_token_set"], true);
        // A channel nobody configured: the sentence, not an error.
        let v = json_of(
            app.oneshot(with_body(
                "POST",
                "/api/v1/settings/notify/test",
                &token,
                serde_json::json!({ "channel": "unifiedpush" }),
            ))
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(v["sent"], false);
        assert!(v["error"].is_string(), "{v}");
    }

    #[tokio::test]
    async fn the_machine_the_report_and_the_status_extras() {
        let (app, token, _core, _aid) = learning_base().await;
        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/insights/machine", &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            v["artifacts"].is_number() && v["jobs"].is_array() && v["sweep_history"].is_array(),
            "{v}"
        );
        let v = json_of(
            app.clone()
                .oneshot(get("/api/v1/insights/report", &token))
                .await
                .unwrap(),
        )
        .await;
        assert!(v.get("sleep").is_some() && v.get("evolve").is_some(), "{v}");
        assert!(
            v["pursuits"].is_array(),
            "learning is on, so the line is there: {v}"
        );
        assert!(v["more_pairs"].is_number());

        let v = json_of(
            app.oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .header("authorization", format!("Bearer {token}"))
                    .header("accept-language", "de-DE,de;q=0.9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(v["learn"], true, "{v}");
        assert!(v["asks"].is_boolean() && v["vision"].is_boolean() && v["recommend"].is_boolean());
        assert_eq!(v["held"]["corpora"], 1);
        assert!(v["last_kept"]["id"].is_string(), "{v}");
        assert_eq!(
            v["examples"]["lang"], "de",
            "the phrasings follow Accept-Language: {v}"
        );
        assert_eq!(v["teach"], true, "one source is a young base");
    }
}
