//! Turning captured searches into labelled pairs.
//!
//! The card shows the query as it was typed and the stored pool shuffled, with
//! no ranks and no scores. Both omissions are deliberate: the ranker's opinion
//! is the one thing that must not be visible while its work is being judged, or
//! what gets measured is agreement rather than relevance.
//!
//! The pool offered is wider than the answer the searcher saw, so an artifact
//! the ranking buried can still be confirmed. That is the only way a ranking
//! failure leaves a record instead of passing as a shrug.

use crate::auth::Identity;
use crate::error::Result;
use crate::store::artifacts::ArtifactStatus;
use crate::store::feedback::{PendingEvent, Stats, Verdict};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};

/// Judgements before the first parameter sweep can say anything.
///
/// Below this a proposal is noise: half a dozen queries cannot separate a real
/// improvement from the quirks of half a dozen queries. The tuning plan replaces
/// this constant with `feedback.tune.min_judgements`.
pub const FIRST_SWEEP_AT: i64 = 50;

/// Judgements before the miss list is worth opening.
const MISS_LIST_AT: i64 = 10;

pub struct Choice {
    pub artifact_id: String,
    pub title: String,
    pub snippet: String,
    /// Whether confirming this one would produce a pair the benchmark can hold.
    /// A deprecated or superseded artifact is offered but not choosable — see
    /// `card_for` for why it is shown at all.
    pub usable: bool,
    /// The digit that presses this option, or `None` where no key reaches it:
    /// past the ninth, or on something unusable. Assigned after the shuffle,
    /// over the choosable options only, so the digits an operator can see are
    /// the digits that work and they run without a gap.
    pub key: Option<usize>,
}

pub struct Card {
    pub id: String,
    pub query: String,
    pub door: String,
    pub when: String,
    pub choices: Vec<Choice>,
}

#[derive(Template)]
#[template(path = "judge.html")]
struct JudgeTemplate {
    /// The layout stamps this on `<html>`; every full page carries it.
    theme: String,
    /// Waiting judgements for the nav. See `state::judge_pending`. Counted on
    /// this page too, so the badge falls as the queue is worked down rather
    /// than standing at whatever it read on arrival.
    judge_pending: Option<i64>,
    stats: Stats,
    recall: String,
    mrr: String,
    target: i64,
    progress_pct: i64,
    misses: Vec<crate::store::feedback::Miss>,
    card: Option<Card>,
    /// Always `None` here — the page is a fresh arrival, not the moment after a
    /// verdict. It exists because the card partial is shared with the fragment
    /// route, which does show one.
    flash: Option<Flash>,
}

#[derive(Template)]
#[template(path = "_judge_card.html")]
struct CardTemplate {
    card: Option<Card>,
    /// What the judgement just before this one revealed. `None` on a plain
    /// fetch of the next card.
    flash: Option<Flash>,
}

pub struct Flash {
    pub line: String,
    /// `MRR 0.54 → 0.57`, so the figure the work is measured by visibly moves
    /// as the work is done.
    pub delta: String,
    /// The event this verdict was recorded against, so it can be taken back.
    /// `None` after a skip, which recorded nothing to undo.
    pub undo: Option<String>,
}

/// What the judgement just revealed, said plainly.
///
/// The emphasis runs opposite to intuition: the better the ranking did, the
/// quieter the line. A rank-one confirmation teaches almost nothing, and an
/// interface that cheers for it is training its operator to agree with
/// whatever was already on top.
pub fn diagnosis(rank: Option<i64>, verdict: Verdict) -> &'static str {
    match (verdict, rank) {
        (Verdict::Gap, _) => "a hole: your base doesn't know this yet.",
        (Verdict::Discard, _) => "discarded.",
        (Verdict::Hit, None) => "a find: search would never have shown you this.",
        (Verdict::Hit, Some(r)) if r >= 10 => {
            "the ranking got this wrong — this is what we're here for."
        }
        (Verdict::Hit, Some(r)) if r > 0 => "there, but far down. These are what move the MRR.",
        (Verdict::Hit, _) => "found as expected.",
    }
}

/// Roughly how long ago, in the words someone would use out loud. Precision
/// past "days" would suggest the timestamp matters; it is here to jog a memory.
fn ago(then: i64) -> String {
    let days = (crate::store::now() - then).max(0) / 86_400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        n if n < 30 => format!("{n} days ago"),
        n => format!("{} months ago", n / 30),
    }
}

fn snippet_of(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(140) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// Shuffle without pulling in a random-number crate: the event id is already a
/// uuid v7, so hashing it together with each artifact id gives an order that is
/// stable for one card, different for the next, and unrelated to rank.
fn shuffled(event_id: &str, mut choices: Vec<Choice>) -> Vec<Choice> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    choices.sort_by_key(|c| {
        let mut h = DefaultHasher::new();
        event_id.hash(&mut h);
        c.artifact_id.hash(&mut h);
        h.finish()
    });
    choices
}

/// Hydrate a pending event into something renderable, dropping candidates whose
/// artifact has since been deleted and marking those the benchmark cannot hold.
///
/// One read per candidate rather than one query for all of them: the pool is at
/// most `feedback.candidates` long, this is not a hot path, and a hand-built
/// `IN (?, ?, …)` would be the more fragile of the two.
async fn card_for(st: &AppState, event: PendingEvent) -> Result<Card> {
    let mut choices = Vec::with_capacity(event.candidates.len());
    for c in &event.candidates {
        // A deleted artifact keeps its candidate row — the pool is history —
        // but it cannot be offered as something to choose. Only that: any other
        // failure is raised, because a pool quietly one short is one the
        // operator judges anyway, and the verdict is recorded as though the
        // missing candidate had been seen and rejected.
        match st.core.store.get_artifact(&c.artifact_id).await {
            Ok(a) => choices.push(Choice {
                // Deprecated and superseded artifacts are shown greyed rather
                // than dropped, for the reason just given: shortening the pool
                // silently is what makes a verdict mean something it doesn't.
                // `hit` refuses these anyway — `eval::export` would drop the
                // pair — so showing them unchoosable says the same thing on the
                // card, before the keystroke, instead of after it.
                usable: a.status == ArtifactStatus::Active && a.superseded_by.is_none(),
                artifact_id: a.id,
                title: a.title.unwrap_or_else(|| "Untitled".into()),
                snippet: snippet_of(&a.text),
                key: None,
            }),
            Err(crate::error::Error::NotFound) => continue,
            Err(e) => return Err(e),
        }
    }
    let mut choices = shuffled(&event.id, choices);
    // After the shuffle, because the digits number the card as it is read. The
    // range is the cap: the shortcut is one digit, so the tenth choosable
    // option and everything after it keeps its column and loses the number.
    for (key, c) in (1..=9).zip(choices.iter_mut().filter(|c| c.usable)) {
        c.key = Some(key);
    }
    Ok(Card {
        choices,
        id: event.id,
        query: event.query,
        door: event.door,
        when: ago(event.created_at),
    })
}

async fn next_pending_card(st: &AppState) -> Result<Option<Card>> {
    match st.core.store.next_pending().await? {
        Some(event) => Ok(Some(card_for(st, event).await?)),
        None => Ok(None),
    }
}

async fn page(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    use axum::response::IntoResponse;
    let stats = st.core.store.feedback_stats().await?;
    let misses = if stats.judged >= MISS_LIST_AT {
        st.core.store.misses(20).await?
    } else {
        vec![]
    };
    let progress_pct = (stats.judged * 100 / FIRST_SWEEP_AT.max(1)).min(100);
    Ok(HtmlTemplate(JudgeTemplate {
        theme: "light".into(),
        // Read off the stats already in hand rather than counted again.
        judge_pending: st.core.feedback.enabled.then_some(stats.pending),
        recall: format!("{:.2}", stats.recall_at_10),
        mrr: format!("{:.2}", stats.mrr),
        target: FIRST_SWEEP_AT,
        progress_pct,
        stats,
        misses,
        card: next_pending_card(&st).await?,
        flash: None,
    })
    .into_response())
}

async fn next_card(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    use axum::response::IntoResponse;
    Ok(HtmlTemplate(CardTemplate {
        card: next_pending_card(&st).await?,
        flash: None,
    })
    .into_response())
}

/// Render the next card with a note about the verdict that was just given.
///
/// The MRR is read on both sides of the write, so the delta shown is the one
/// this judgement actually caused rather than a figure recomputed later.
async fn card_after(
    st: &AppState,
    before: f64,
    line: &'static str,
    judged: &str,
) -> Result<Response> {
    use axum::response::IntoResponse;
    let after = st.core.store.feedback_stats().await?.mrr;
    Ok(HtmlTemplate(CardTemplate {
        card: next_pending_card(st).await?,
        flash: Some(Flash {
            line: line.to_string(),
            delta: format!("MRR {before:.2} → {after:.2}"),
            undo: Some(judged.to_string()),
        }),
    })
    .into_response())
}

/// Put the same card back with a note, having recorded nothing.
///
/// Not `card_after` with an empty delta: nothing was judged, so there is no MRR
/// movement to report and nothing to undo. The event is still pending, so it is
/// fetched by id rather than taken from the queue — a capture landing in the
/// meantime would otherwise swap the card out from under the correction.
async fn card_again(st: &AppState, event_id: &str, line: &str) -> Result<Response> {
    use axum::response::IntoResponse;
    let card = match st.core.store.pending_by_id(event_id).await? {
        Some(event) => Some(card_for(st, event).await?),
        None => next_pending_card(st).await?,
    };
    Ok(HtmlTemplate(CardTemplate {
        card,
        flash: Some(Flash {
            line: line.to_string(),
            delta: String::new(),
            undo: None,
        }),
    })
    .into_response())
}

/// One candidate's full text, for reading before confirming it.
///
/// The snippet on the card is 140 characters, which is enough to recognise an
/// artifact and not enough to be sure of one — and a verdict is a line in the
/// dataset the ranker is scored against. Deliberately says nothing about rank,
/// score or whether the search showed this at all: the card withholds that on
/// purpose, and a detail view that leaked it would undo the whole arrangement.
async fn read_artifact(
    State(st): State<AppState>,
    _id: Identity,
    Path(artifact_id): Path<String>,
) -> Result<Response> {
    use axum::response::IntoResponse;
    let a = st.core.store.get_artifact(&artifact_id).await?;
    Ok(HtmlTemplate(FullTemplate {
        html: crate::web::markdown::render(&a.text),
    })
    .into_response())
}

/// Take back the verdict just recorded and return to that card.
///
/// The keyboard shortcuts make judging fast enough to be done at all, and fast
/// enough to misfire; without this, a slipped digit is a wrong pair scored as
/// truth forever. The event comes back to the card it was on rather than to
/// whatever now heads the queue.
async fn undo(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
) -> Result<Response> {
    use axum::response::IntoResponse;
    // The event may have expired between the verdict and the second thoughts.
    // The store says so now rather than reporting a write it did not make; here
    // that is not an error, for the reason below.
    match st.core.store.unjudge(&event_id).await {
        Ok(()) | Err(crate::error::Error::NotFound) => {}
        Err(e) => return Err(e),
    }
    let card = match st.core.store.pending_by_id(&event_id).await? {
        Some(event) => Some(card_for(&st, event).await?),
        // Expired out from under the operator, or never existed. The next card
        // is a better answer than an error page.
        None => next_pending_card(&st).await?,
    };
    Ok(HtmlTemplate(CardTemplate { card, flash: None }).into_response())
}

#[derive(serde::Deserialize)]
pub struct HitForm {
    pub artifact_id: String,
}

async fn hit(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
    axum::extract::Form(f): axum::extract::Form<HitForm>,
) -> Result<Response> {
    // The id has to name something. Both paths that post here — the card and
    // the assign search — offer only artifacts that existed when they were
    // rendered, so a miss means the artifact was deleted since, or the form was
    // replayed. Either way the expectation would name nothing: `feedback_stats`
    // reads a missing rank as "search never showed you this", counts the event
    // as a find, and drags recall@10 down for a ranking failure that never
    // happened. Pool membership is deliberately not required — an artifact the
    // search never offered is exactly what the assign path is for.
    let artifact = st.core.store.get_artifact(&f.artifact_id).await?;

    // Being active is required, though. `eval::export` freezes only active,
    // un-superseded artifacts and drops any pair naming something else, so a
    // confirmation here would raise the recall and MRR on this very page while
    // contributing nothing to `pairs.json` — the two numbers the operator is
    // asked to trust, disagreeing about the same judgement. Refused rather than
    // recorded: the card comes back so the answer can be given again against
    // something the benchmark will still be able to hold.
    if artifact.status != ArtifactStatus::Active || artifact.superseded_by.is_some() {
        return card_again(
            &st,
            &event_id,
            "that one is deprecated or superseded, so the benchmark can't hold it. \
             Pick what answers this now, or call it a gap.",
        )
        .await;
    }

    // Read before the write: afterwards the event is no longer pending, and the
    // rank is what decides which diagnosis the operator gets.
    let rank = st
        .core
        .store
        .rank_in_event(&event_id, &f.artifact_id)
        .await?;
    let before = st.core.store.feedback_stats().await?.mrr;
    st.core.store.judge_hit(&event_id, &f.artifact_id).await?;
    card_after(&st, before, diagnosis(rank, Verdict::Hit), &event_id).await
}

async fn gap(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
) -> Result<Response> {
    let before = st.core.store.feedback_stats().await?.mrr;
    st.core.store.judge(&event_id, Verdict::Gap).await?;
    card_after(&st, before, diagnosis(None, Verdict::Gap), &event_id).await
}

async fn discard(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
) -> Result<Response> {
    let before = st.core.store.feedback_stats().await?.mrr;
    st.core.store.judge(&event_id, Verdict::Discard).await?;
    card_after(&st, before, diagnosis(None, Verdict::Discard), &event_id).await
}

async fn skip(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
) -> Result<Response> {
    st.core.store.skip_event(&event_id).await?;
    next_card(State(st), _id).await
}

// ── The "none of these" path ────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "_judge_full.html")]
struct FullTemplate {
    /// Rendered and sanitized markdown — chunk text is model output shown
    /// inside an authenticated session, so it is untrusted by definition.
    html: String,
}

#[derive(Template)]
#[template(path = "_judge_assign.html")]
struct AssignTemplate {
    event_id: String,
    /// The query as it was captured — the thing being judged. Fixed for the
    /// life of the screen: it is the operator's reference for what they are
    /// looking for, so typing must not overwrite it.
    event_query: String,
    /// What is in the search box. Separate from `event_query` because the swap
    /// rebuilds the input, and a box that renders empty loses whatever was
    /// being typed.
    typed: String,
    results: Vec<Choice>,
    /// Whether a search has been run yet, so an empty list can say "nothing
    /// matched" instead of appearing before anything was asked.
    searched: bool,
}

async fn assign(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
) -> Result<Response> {
    use axum::response::IntoResponse;
    // By id, not by "whichever is next to judge": a capture landing between the
    // card being drawn and this click would otherwise win the ordering and
    // leave the screen with no query on it at all.
    let event_query = st
        .core
        .store
        .event_query(&event_id)
        .await?
        .unwrap_or_default();
    Ok(HtmlTemplate(AssignTemplate {
        event_id,
        event_query,
        typed: String::new(),
        results: vec![],
        searched: false,
    })
    .into_response())
}

#[derive(serde::Deserialize)]
pub struct AssignQuery {
    #[serde(default)]
    pub q: String,
}

async fn assign_results(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
    axum::extract::Query(p): axum::extract::Query<AssignQuery>,
) -> Result<Response> {
    use axum::response::IntoResponse;
    let event_query = st
        .core
        .store
        .event_query(&event_id)
        .await?
        .unwrap_or_default();
    let mut results = vec![];
    if !p.q.trim().is_empty() {
        let query = crate::core::search::SearchQuery {
            q: p.q.clone(),
            limit: 10,
            tags: vec![],
            category: None,
            // Looking something up in order to label it is not the operator
            // reading their notes.
            mark: false,
            include_deprecated: false,
            include_superseded: false,
        };
        // The one search in the application that must never be captured: it is
        // composed in full knowledge of the answer, which is the contamination
        // the whole feature exists to keep out of the dataset.
        let hits = st
            .core
            .search(&query, crate::store::feedback::Door::Judge)
            .await?;
        results = hits
            .into_iter()
            .enumerate()
            .map(|(i, h)| Choice {
                artifact_id: h.artifact_id,
                title: h.title.unwrap_or_else(|| "Untitled".into()),
                snippet: snippet_of(&h.text),
                // The search that produced these excluded deprecated and
                // superseded artifacts, so everything offered here is something
                // the benchmark can hold.
                usable: true,
                // `limit` above is ten and the shortcut is one digit, so the
                // tenth result gets no badge rather than one that cannot be
                // pressed. Numbered here rather than in the template for the
                // same reason as the card: the digits are the ones that work.
                key: (i < 9).then_some(i + 1),
            })
            .collect();
    }
    Ok(HtmlTemplate(AssignTemplate {
        event_id,
        event_query,
        typed: p.q,
        results,
        searched: true,
    })
    .into_response())
}

pub fn judge_router() -> Router<AppState> {
    Router::new()
        .route("/ui/judge", get(page))
        .route("/ui/judge/next", get(next_card))
        .route("/ui/judge/{id}/hit", post(hit))
        .route("/ui/judge/{id}/gap", post(gap))
        .route("/ui/judge/{id}/discard", post(discard))
        .route("/ui/judge/{id}/skip", post(skip))
        .route("/ui/judge/{id}/assign", get(assign))
        .route("/ui/judge/{id}/assign/results", get(assign_results))
        .route("/ui/judge/{id}/undo", post(undo))
        .route("/ui/judge/read/{artifact_id}", get(read_artifact))
}

#[cfg(test)]
mod tests {
    use crate::store::artifacts::ArtifactStatus;
    use crate::store::feedback::{Door, NewCandidate, NewEvent};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// A session, `real` genuine artifacts, and one captured search whose pool
    /// is those artifacts followed by `phantom` ids that name nothing.
    async fn judge_app(
        real: usize,
        phantom: &[&str],
    ) -> (axum::Router, String, crate::core::Core, Vec<String>) {
        let core = crate::core::test_support::test_core().await;
        let cid = crate::store::new_id();
        core.store
            .insert_session(&cid, "user-1", None, 3600)
            .await
            .unwrap();

        let src = core
            .store
            .insert_corpus("raw for judging", "web", None)
            .await
            .unwrap();
        let new: Vec<crate::store::artifacts::NewArtifact> = (0..real)
            .map(|i| crate::store::artifacts::NewArtifact {
                ordinal: i as i64,
                text: format!("artifact number {i}, about mounting an image"),
                corpus_span: None,
                title: Some(format!("artifact {i}")),
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        let ids: Vec<String> = made.iter().map(|c| c.id.clone()).collect();

        let mut pool: Vec<String> = ids.clone();
        pool.extend(phantom.iter().map(|s| s.to_string()));
        if !pool.is_empty() {
            core.store
                .record_search(
                    NewEvent {
                        query: "the image will not mount".into(),
                        door: Door::Ui,
                        scope: None,
                        filters: "{}".into(),
                        query_vec: vec![0.1, 0.2],
                        embed_model: "fake".into(),
                        candidates: pool
                            .iter()
                            .enumerate()
                            .map(|(i, id)| NewCandidate {
                                artifact_id: id.clone(),
                                score: 1.0 - i as f32 / 100.0,
                                similarity: Some(0.5),
                                shown: i < 10,
                            })
                            .collect(),
                    },
                    0,
                )
                .await
                .unwrap();
        }

        let handle = core.clone();
        let state = crate::web::state::AppState {
            core,
            auth: std::sync::Arc::new(crate::web::state::AuthContext {
                mode: crate::config::AuthMode::Local,
                local: None,
                oidc: None,
                pending: crate::auth::oidc::PendingStore::new(),
                secure_cookies: false,
            }),
        };
        (
            crate::web::router(state),
            format!("engram_session={cid}"),
            handle,
            ids,
        )
    }

    async fn body_of(res: axum::response::Response) -> String {
        let b = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8_lossy(&b).to_string()
    }

    async fn get(app: &axum::Router, uri: &str, cookie: &str) -> String {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        body_of(res).await
    }

    async fn post(app: &axum::Router, uri: &str, cookie: &str, body: &str) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .method("POST")
                    .header("cookie", cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn the_card_offers_the_whole_pool_not_only_what_was_shown() {
        // Offering only the ten that were displayed would make a buried hit
        // unconfirmable, and the ranking failure invisible.
        let (app, cookie, _core, ids) = judge_app(13, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        for id in &ids {
            assert!(body.contains(id.as_str()), "candidate {id} missing");
        }
    }

    #[tokio::test]
    async fn only_the_options_a_key_can_reach_are_numbered() {
        // The shortcut is a single digit and the pool is twenty deep by
        // default, so past the ninth the badge advertised a key that does
        // nothing — half the card looking operable and answering to nothing.
        let (app, cookie, _core, _) = judge_app(13, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;

        assert!(body.contains(r#"<span class="judge-key">9</span>"#));
        assert!(
            !body.contains(r#"<span class="judge-key">10</span>"#),
            "the tenth option offers a key that cannot be pressed"
        );
        assert_eq!(
            body.matches(r#"<span class="judge-key"></span>"#).count(),
            4,
            "the options past the ninth must keep their column and lose the number"
        );
    }

    #[tokio::test]
    async fn the_card_shows_no_ranks_and_no_scores() {
        // Both are the ranker's opinion, which is exactly what must not be
        // heard while judging.
        let (app, cookie, _core, _) = judge_app(3, &[]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        assert!(!body.contains("rank"), "a rank leaked into the card");
        assert!(!body.contains("score"), "a score leaked into the card");
    }

    #[tokio::test]
    async fn confirming_a_candidate_records_the_hit_and_moves_on() {
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        let event = core.store.next_pending().await.unwrap().unwrap();
        let status = post(
            &app,
            &format!("/ui/judge/{}/hit", event.id),
            &cookie,
            &format!("artifact_id={}", ids[1]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let s = core.store.feedback_stats().await.unwrap();
        assert_eq!(s.hits, 1);
        assert!(core.store.next_pending().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_deprecated_candidate_cannot_be_confirmed() {
        // `eval::export` freezes only active artifacts and drops any pair
        // naming something else. Recording this would raise the recall and MRR
        // on this very page while `pairs.json` gained nothing — the two numbers
        // the operator is asked to trust, disagreeing about one judgement.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        core.store
            .set_artifact_status(&ids[1], ArtifactStatus::Deprecated)
            .await
            .unwrap();
        let event = core.store.next_pending().await.unwrap().unwrap();

        let status = post(
            &app,
            &format!("/ui/judge/{}/hit", event.id),
            &cookie,
            &format!("artifact_id={}", ids[1]),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the operator gets the card back");

        assert_eq!(core.store.feedback_stats().await.unwrap().hits, 0);
        assert_eq!(
            core.store.next_pending().await.unwrap().map(|e| e.id),
            Some(event.id),
            "the event must still be waiting for a verdict it can keep"
        );
    }

    #[tokio::test]
    async fn a_deprecated_candidate_is_shown_unchoosable_rather_than_offered() {
        // The refusal above is correct but arrives too late to be read: the
        // shuffle is seeded by event id, so the rejected option came back at
        // the same place with the same digit and nothing marking it, and an
        // operator judging by keystroke got the same flash every time. The
        // pool still shows at full length — a card quietly one option short is
        // one where "none of these" means something it doesn't — but the
        // option carries no key and cannot be posted.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        core.store
            .set_artifact_status(&ids[1], ArtifactStatus::Deprecated)
            .await
            .unwrap();

        let body = get(&app, "/ui/judge/next", &cookie).await;
        assert!(
            body.contains(ids[1].as_str()) || body.contains("judge-option-unusable"),
            "the deprecated candidate must still appear in the pool"
        );
        assert!(
            body.contains("judge-option-unusable") && body.contains("disabled"),
            "it must be marked and disabled rather than silently refused later"
        );
        assert_eq!(
            body.matches(r#"<span class="judge-key">1</span>"#).count(),
            1,
            "the one choosable option keeps the first digit"
        );
        assert_eq!(
            body.matches(r#"<span class="judge-key">2</span>"#).count(),
            0,
            "no digit may point at an option that would be refused"
        );
    }

    #[tokio::test]
    async fn a_candidate_can_be_read_in_full_before_it_is_confirmed() {
        // The snippet stops at 140 characters, and the click after it writes a
        // line into the dataset the ranker is scored against.
        let (app, cookie, core, ids) = judge_app(1, &[]).await;
        let card = get(&app, "/ui/judge/next", &cookie).await;
        assert!(
            card.contains(&format!("/ui/judge/read/{}", ids[0])),
            "the card offers no way to read a candidate: {card}"
        );

        let full = get(&app, &format!("/ui/judge/read/{}", ids[0]), &cookie).await;
        let stored = core.store.get_artifact(&ids[0]).await.unwrap();
        assert!(
            full.contains(&stored.text),
            "the reading view is not the artifact: {full}"
        );
        // Reading must stay a read: the event is still waiting for a verdict.
        assert!(core.store.next_pending().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_reading_view_says_nothing_about_rank_or_score() {
        let (app, cookie, _core, ids) = judge_app(1, &[]).await;
        let full = get(&app, &format!("/ui/judge/read/{}", ids[0]), &cookie).await;
        assert!(
            !full.contains("rank"),
            "a rank leaked into the reading view"
        );
        assert!(
            !full.contains("score"),
            "a score leaked into the reading view"
        );
    }

    #[tokio::test]
    async fn a_verdict_can_be_taken_back() {
        // Judging is driven by digit keys because it has to cost seconds, and
        // that is exactly what makes it misfire. A pair labelled by a slipped
        // key is scored as truth.
        let (app, cookie, core, ids) = judge_app(2, &[]).await;
        let event = core.store.next_pending().await.unwrap().unwrap();
        let flash = {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/ui/judge/{}/hit", event.id))
                        .method("POST")
                        .header("cookie", &cookie)
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from(format!("artifact_id={}", ids[0])))
                        .unwrap(),
                )
                .await
                .unwrap();
            body_of(res).await
        };
        assert!(
            flash.contains(&format!("/ui/judge/{}/undo", event.id)),
            "the verdict was recorded with no way back: {flash}"
        );
        assert_eq!(core.store.feedback_stats().await.unwrap().hits, 1);

        let back = {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/ui/judge/{}/undo", event.id))
                        .method("POST")
                        .header("cookie", &cookie)
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            body_of(res).await
        };

        let s = core.store.feedback_stats().await.unwrap();
        assert_eq!((s.hits, s.judged), (0, 0), "the verdict outlived its undo");
        let pending = core.store.next_pending().await.unwrap().unwrap();
        assert_eq!(pending.id, event.id, "a different event came back");
        assert!(
            back.contains(&event.id),
            "undo did not return to the card it undid: {back}"
        );
        // The answer goes with the verdict: a stale `expect_id` would keep
        // counting towards recall for a judgement nobody stands behind.
        assert_eq!(
            core.store.rank_in_event(&event.id, &ids[0]).await.unwrap(),
            Some(0),
            "the pool is history and must survive the undo"
        );
        assert_eq!(
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT expect_id FROM search_events WHERE id = ?"
            )
            .bind(&event.id)
            .fetch_one(&core.store.pool)
            .await
            .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn skipping_leaves_it_pending() {
        let (app, cookie, core, _) = judge_app(1, &[]).await;
        let event = core.store.next_pending().await.unwrap().unwrap();
        post(&app, &format!("/ui/judge/{}/skip", event.id), &cookie, "").await;

        assert!(core.store.next_pending().await.unwrap().is_some());
        assert_eq!(core.store.feedback_stats().await.unwrap().judged, 0);
    }

    #[tokio::test]
    async fn a_vanished_artifact_is_left_out_of_the_card() {
        // The pool is history and keeps its rows; the card is a list of things
        // that can still be chosen.
        let (app, cookie, _core, _) = judge_app(1, &["gone-for-good"]).await;
        let body = get(&app, "/ui/judge/next", &cookie).await;
        assert!(!body.contains("gone-for-good"));
    }

    #[tokio::test]
    async fn confirming_an_artifact_that_no_longer_exists_records_nothing() {
        // A card drawn before the artifact was deleted, or a replayed POST.
        // The expectation would name nothing, and a missing rank reads as "the
        // search would never have shown you this" — a find, and a permanent
        // dent in recall@10 for a ranking failure that never happened.
        let (app, cookie, core, _) = judge_app(1, &["gone-for-good"]).await;
        let event = core.store.next_pending().await.unwrap().unwrap();
        let status = post(
            &app,
            &format!("/ui/judge/{}/hit", event.id),
            &cookie,
            "artifact_id=gone-for-good",
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let s = core.store.feedback_stats().await.unwrap();
        assert_eq!(s.finds, 0, "a phantom was counted as a find");
        assert_eq!(s.judged, 0);
        assert!(
            core.store.next_pending().await.unwrap().is_some(),
            "the event was consumed by a verdict that was refused"
        );
    }

    #[tokio::test]
    async fn a_verdict_on_a_purged_event_says_so_instead_of_claiming_success() {
        // Retention or an Ops purge under an open judging screen. The flash
        // would otherwise show an MRR delta and an Undo for a row that is gone.
        let (app, cookie, core, ids) = judge_app(1, &[]).await;
        let event = core.store.next_pending().await.unwrap().unwrap();
        core.store.purge_feedback().await.unwrap();

        for (uri, body) in [
            (
                format!("/ui/judge/{}/hit", event.id),
                format!("artifact_id={}", ids[0]),
            ),
            (format!("/ui/judge/{}/gap", event.id), String::new()),
            (format!("/ui/judge/{}/discard", event.id), String::new()),
            (format!("/ui/judge/{}/skip", event.id), String::new()),
        ] {
            assert_eq!(
                post(&app, &uri, &cookie, &body).await,
                StatusCode::NOT_FOUND,
                "{uri} claimed to record a verdict on an event that is gone"
            );
        }
    }

    #[test]
    fn the_diagnosis_is_loudest_where_the_ranking_did_worst() {
        // Inverted on purpose. A first-position hit is the least informative
        // card of the day; making it the most celebrated would breed agreement
        // with whatever the ranker already thought.
        use super::diagnosis;
        use crate::store::feedback::Verdict;
        assert_eq!(diagnosis(Some(0), Verdict::Hit), "found as expected.");
        assert!(diagnosis(Some(13), Verdict::Hit).contains("wrong"));
        assert!(diagnosis(None, Verdict::Hit).contains("find"));
        assert!(diagnosis(None, Verdict::Gap).contains("hole"));
    }

    #[tokio::test]
    async fn the_assignment_search_is_never_captured() {
        // It is composed in full knowledge of the answer. Recording it would
        // feed the dataset exactly the contamination this feature avoids.
        let (app, cookie, core, _) = judge_app(2, &[]).await;
        core.store.purge_feedback().await.unwrap();
        let event = core
            .store
            .record_search(
                NewEvent {
                    query: "the one being judged".into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![],
                },
                0,
            )
            .await
            .unwrap();
        let before = core.store.feedback_stats().await.unwrap().captured;

        get(
            &app,
            &format!("/ui/judge/{event}/assign/results?q=mounting+an+image"),
            &cookie,
        )
        .await;
        core.background.wait_idle().await;

        assert_eq!(
            core.store.feedback_stats().await.unwrap().captured,
            before,
            "looking something up in order to label it must not become data"
        );
    }

    #[tokio::test]
    async fn confirming_from_outside_the_pool_is_reported_as_a_find() {
        let (app, cookie, core, ids) = judge_app(1, &[]).await;
        let event = core.store.next_pending().await.unwrap().unwrap();
        // An artifact that exists but was never in this event's pool.
        let src = core
            .store
            .insert_corpus("another raw", "web", None)
            .await
            .unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "the artifact search never offered".into(),
                    corpus_span: None,
                    title: Some("unoffered".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        assert_ne!(made[0].id, ids[0]);

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/judge/{}/hit", event.id))
                    .method("POST")
                    .header("cookie", &cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("artifact_id={}", made[0].id)))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_of(res).await;
        assert!(body.contains("a find"), "the flash did not name it a find");
        assert_eq!(core.store.feedback_stats().await.unwrap().finds, 1);
    }

    #[tokio::test]
    async fn nothing_pending_says_so_rather_than_rendering_an_empty_card() {
        let (app, cookie, _core, _) = judge_app(0, &[]).await;
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(
            body.to_lowercase().contains("nothing to judge"),
            "an empty queue must say so"
        );
    }
}
