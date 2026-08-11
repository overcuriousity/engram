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
    stats: Stats,
    recall: String,
    mrr: String,
    target: i64,
    progress_pct: i64,
    misses: Vec<crate::store::feedback::Miss>,
    card: Option<Card>,
}

#[derive(Template)]
#[template(path = "_judge_card.html")]
struct CardTemplate {
    card: Option<Card>,
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
/// artifact has since been deleted.
///
/// One read per candidate rather than one query for all of them: the pool is at
/// most `feedback.candidates` long, this is not a hot path, and a hand-built
/// `IN (?, ?, …)` would be the more fragile of the two.
async fn card_for(st: &AppState, event: PendingEvent) -> Result<Card> {
    let mut choices = Vec::with_capacity(event.candidates.len());
    for c in &event.candidates {
        // A deleted artifact keeps its candidate row — the pool is history —
        // but it cannot be offered as something to choose.
        if let Ok(a) = st.core.store.get_artifact(&c.artifact_id).await {
            choices.push(Choice {
                artifact_id: a.id,
                title: a.title.unwrap_or_else(|| "Untitled".into()),
                snippet: snippet_of(&a.text),
            });
        }
    }
    Ok(Card {
        choices: shuffled(&event.id, choices),
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
        recall: format!("{:.2}", stats.recall_at_10),
        mrr: format!("{:.2}", stats.mrr),
        target: FIRST_SWEEP_AT,
        progress_pct,
        stats,
        misses,
        card: next_pending_card(&st).await?,
    })
    .into_response())
}

async fn next_card(State(st): State<AppState>, _id: Identity) -> Result<Response> {
    use axum::response::IntoResponse;
    Ok(HtmlTemplate(CardTemplate {
        card: next_pending_card(&st).await?,
    })
    .into_response())
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
    st.core.store.judge_hit(&event_id, &f.artifact_id).await?;
    next_card(State(st), _id).await
}

async fn gap(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
) -> Result<Response> {
    st.core.store.judge(&event_id, Verdict::Gap).await?;
    next_card(State(st), _id).await
}

async fn discard(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
) -> Result<Response> {
    st.core.store.judge(&event_id, Verdict::Discard).await?;
    next_card(State(st), _id).await
}

async fn skip(
    State(st): State<AppState>,
    _id: Identity,
    Path(event_id): Path<String>,
) -> Result<Response> {
    st.core.store.skip_event(&event_id).await?;
    next_card(State(st), _id).await
}

pub fn judge_router() -> Router<AppState> {
    Router::new()
        .route("/ui/judge", get(page))
        .route("/ui/judge/next", get(next_card))
        .route("/ui/judge/{id}/hit", post(hit))
        .route("/ui/judge/{id}/gap", post(gap))
        .route("/ui/judge/{id}/discard", post(discard))
        .route("/ui/judge/{id}/skip", post(skip))
}

#[cfg(test)]
mod tests {
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
    async fn nothing_pending_says_so_rather_than_rendering_an_empty_card() {
        let (app, cookie, _core, _) = judge_app(0, &[]).await;
        let body = get(&app, "/ui/judge", &cookie).await;
        assert!(
            body.to_lowercase().contains("nothing to judge"),
            "an empty queue must say so"
        );
    }
}
