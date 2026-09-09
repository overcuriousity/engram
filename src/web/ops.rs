//! The review queue and the operator's hand on the corpus.
//!
//! The last screen out of `web::ui`. Everything under `/ui/ops`: the pairs a
//! sweep queued for a person to rule on, and the buttons that act on one
//! artifact — supersede and take it back, deprecate and reactivate, verify,
//! undo a merge, undo a condensation, resolve a parked near-duplicate.
//!
//! Every one of these ends by redrawing the artifact pane, which is why this
//! module is a caller of `web::artifact` rather than a peer: `artifact_changed`
//! is the one place that decides between the fragment and a redirect, and
//! `ReturnTo` is what a form carries so a press knows which page it was made
//! from.

use crate::error::Result;
use crate::tenants::Tenant;
use crate::web::artifact::{ArtifactDetailFragment, ArtifactViewParams, build_artifact_detail};
use crate::web::auth_routes::HtmlTemplate;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use axum::Router;
use axum::extract::{Form, Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;

/// How many decisions Capture offers at once, and the order it looks for them
/// in. Confirmed contradictions and judge-proposed supersedes lead: they are
/// the ones that mean something in the base is wrong or stale, rather than
/// merely repeated.
///
/// A rolling window rather than the whole backlog. This is now the app's start
/// page, so every open paid for three fifty-row queries and two point lookups
/// per pair, and a base with real overlap in it rendered a screen of warning
/// boxes above the captures. Deciding one of these makes the next appear, so
/// the cap strands nothing — there is no second page to go and find the rest
/// on, which is the point: Housekeeping is reference, not work.
pub(crate) const PAIR_LIMIT: usize = 5;
const PAIR_STATES: [crate::store::pairs::PairState; 5] = [
    crate::store::pairs::PairState::Contradiction,
    crate::store::pairs::PairState::Superseded,
    // The judge read both and found one artifact should hold what both say. A
    // proposal rather than a merge already applied: see `PairState::Duplicate`
    // for the measurements that took the action off this verdict. The card
    // renders it through the same branch a pending pair uses — "these two cover
    // the same ground" — and the Synthese button is the press that acts on it.
    crate::store::pairs::PairState::Duplicate,
    // Only ever rows an older base filed: a vacuous verdict is now carried
    // out where it is found (`jobs::dedupe::discard_both`) and its pair
    // settles `Dismissed`. Still listed, because those rows are a
    // recommendation nobody has pressed yet, and without this key they are on
    // no queue at all.
    crate::store::pairs::PairState::Vacuous,
    crate::store::pairs::PairState::Pending,
];

#[derive(serde::Deserialize)]
struct ResolveForm {
    action: crate::core::ingest::NearDupeAction,
}

async fn resolve_near_dupe_ui(
    tenant: Tenant,
    Path(cid): Path<String>,
    Form(form): Form<ResolveForm>,
) -> UiResult<Response> {
    tenant
        .core
        .resolve_near_duplicate(&cid, form.action)
        .await?;
    Ok(Redirect::to("/ui/insights").into_response())
}

/// Where a lifecycle button should land afterwards.
///
/// The same four actions are offered from two places: the Insights review lists,
/// where the queue is the thing being worked through, and an artifact's own
/// page, where being thrown onto Ops for pressing "Confirm still accurate"
/// loses the reader's place. The page that rendered the button says where it
/// leads; Ops sends nothing and keeps the default.
#[derive(serde::Deserialize, Default)]
struct ReturnTo {
    to: Option<String>,
}

impl ReturnTo {
    /// Only a path inside this UI. A form field is user input, and a redirect
    /// that will follow anything it is handed is an open redirect — worth
    /// nothing to the operator and a phishing hop to everyone else.
    fn path(&self) -> &str {
        match self.to.as_deref() {
            Some(p) if p.starts_with("/ui/") && !p.starts_with("/ui//") => p,
            _ => "/ui/insights",
        }
    }
}

/// What a lifecycle button answers with: the artifact it just changed.
///
/// These four buttons say something about an artifact, not about the page it is
/// on — so the answer is that artifact, re-rendered where it already was, and
/// nothing navigates. `_artifact_detail.html` is rendered in two places and the
/// hidden `to` beside each button can only name one of them: it named the
/// standalone artifact page, so pressing "Confirm still accurate" on a search
/// result took the whole window there and the results the operator was working
/// through were gone. That is the reason `ReturnTo` exists, arrived at from the
/// other side.
///
/// The redirect is still what a browser without htmx gets, and `to` is still
/// what it follows. Nothing here is the only way any of these buttons work.
async fn artifact_changed(
    tenant: &Tenant,
    headers: &axum::http::HeaderMap,
    aid: &str,
    terms: &str,
    back: &ReturnTo,
) -> UiResult<Response> {
    if headers.contains_key("hx-request") {
        // As above: an action on the artifact redraws the pane at its opening
        // length rather than reconstructing a run nobody passed along.
        let d = build_artifact_detail(&tenant.core, aid, terms).await?;
        return Ok(HtmlTemplate(ArtifactDetailFragment { d }).into_response());
    }
    Ok(Redirect::to(back.path()).into_response())
}

async fn unsupersede_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    tenant.core.unsupersede(&aid).await?;
    tenant
        .core
        .store
        .undo_action_on(
            &aid,
            crate::store::actions::Kind::Supersede,
            crate::store::actions::UndoneBy::Operator,
            "unsuperseded on Insights",
        )
        .await?;
    artifact_changed(&tenant, &headers, &aid, &p.terms, &back).await
}

async fn dismiss_pair_ui(
    tenant: Tenant,
    Path(pid): Path<i64>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    tenant
        .core
        .store
        .set_pair_state(
            pid,
            crate::store::pairs::PairState::Dismissed,
            None,
            crate::store::pairs::DecidedBy::Operator,
        )
        .await?;
    Ok(Redirect::to(back.path()).into_response())
}

/// Answer a pair by retiring both sides.
///
/// Offered on every card, because the judge is not the only reader who can
/// tell: a pair it called a duplicate can still be two artifacts that say
/// nothing. Where the judge *did* say so, `jobs::dedupe` has already carried
/// this out and the pair never reaches the queue — so what this button answers
/// is the pairs it ruled on differently, and the ones it was never asked
/// about.
///
/// The retiring itself is `jobs::dedupe::discard_both`, shared with that path
/// rather than restated here: the two orderings it documents — both sides read
/// before either is retired, side effects before the pair is settled — are the
/// whole of what makes the action safe, and a second copy of them is a second
/// place for one of them to be dropped.
async fn discard_pair_ui(
    tenant: Tenant,
    Path(pid): Path<i64>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    let pair = tenant.core.store.get_pair(pid).await?;
    let detail = pair.detail.clone();
    crate::jobs::dedupe::discard_both(
        &tenant.core,
        &pair,
        detail.as_deref(),
        crate::store::pairs::DecidedBy::Operator,
    )
    .await?;
    Ok(Redirect::to(back.path()).into_response())
}

/// Which artifact of a pair the operator is keeping. Absent means "whichever
/// the judge proposed", which is what the confirmation button on a proposed
/// supersede sends.
#[derive(serde::Deserialize, Default)]
struct KeepForm {
    keep: Option<String>,
    /// Pressed from Capture, these come back to Capture. Same reasoning as
    /// `ReturnTo`, which validates the path.
    #[serde(flatten)]
    back: ReturnTo,
}

/// Record that this pair should become one artifact, and arm the unit that
/// writes it.
///
/// The press is the judgement. An operator has read both sides and decided they
/// cover the same ground; what is left is the writing, and the writing is an
/// inference call — so it goes where every other inference in this tree goes,
/// onto the job queue, where the budget, the backoff and the attempt count
/// live. Nothing is written here and no model is called: no route under
/// `src/web` calls one, and a handler that blocked on a judge would hold the
/// request open for as long as the endpoint felt like taking.
///
/// Why a person and not the judge: asked twelve times about two artifacts
/// describing one veterinary practice — one carrying the contact details, the
/// other the services — the judge wrote the same reasoning every time and
/// labelled it `distinct` nine times and `duplicate` three. The prompt's own
/// categories both fit that shape, so the label is a coin flip on a decision
/// that hides two artifacts behind a third. The reading is the model's; the
/// call is the operator's.
async fn ask_pair_synthesis_ui(
    tenant: Tenant,
    Path(pid): Path<i64>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    let pair = tenant.core.store.get_pair(pid).await?;
    // The same refusal the Keep buttons make. Every button on this card acts on
    // both sides, and a side already out of results is work nobody can do.
    let (a, b) = (
        tenant.core.store.get_artifact(&pair.a_id).await?,
        tenant.core.store.get_artifact(&pair.b_id).await?,
    );
    if !a.in_results() || !b.in_results() {
        return Err(crate::error::Error::Validation(
            "one of these has already left results".into(),
        )
        .into());
    }
    tenant.core.store.ask_pair_synthesis(pid).await?;
    // Idle-only, like `consolidate` arms it: re-arming a queued unit winds its
    // attempts back to zero.
    tenant
        .core
        .store
        .rearm_idle_seq(
            crate::store::jobs::Stage::Dedupe,
            "pair",
            &pid.to_string(),
            0,
        )
        .await?;
    Ok(Redirect::to(back.path()).into_response())
}

/// Resolve a pair by naming the artifact that survives; the other is superseded
/// by it.
///
/// Two callers, one action. The judge's proposal is a suggestion an operator
/// confirms, and a contradiction the judge could not call is the same decision
/// with nobody suggesting anything — so both are "keep this one", and only the
/// default differs. Before this, a pair the judge flagged as disagreeing but
/// could not rule on offered nothing except Dismiss: the operator could see two
/// artifacts stating different things and had no way to say which was right,
/// so the only way out of the queue was to declare the disagreement uninteresting
/// and leave both in results.
///
/// Nothing before this press hides anything — see `jobs::consolidate::judge_pending`.
async fn apply_pair_supersede_ui(
    tenant: Tenant,
    Path(pid): Path<i64>,
    Form(f): Form<KeepForm>,
) -> UiResult<Response> {
    let pair = tenant.core.store.get_pair(pid).await?;
    // The winner has to be one of this pair's own artifacts. A form field is
    // user input, and superseding an arbitrary id because it arrived in a POST
    // would hide an artifact that has nothing to do with the row that was
    // pressed.
    let obsolete_id = match f.keep {
        Some(keep) if keep == pair.a_id => pair.b_id.clone(),
        Some(keep) if keep == pair.b_id => pair.a_id.clone(),
        Some(_) => {
            return Err(crate::error::Error::Validation(
                "the artifact to keep is not part of this pair".into(),
            )
            .into());
        }
        None => pair
            .obsolete_id
            .clone()
            .ok_or(crate::error::Error::NotFound)?,
    };
    let winner_id = if obsolete_id == pair.a_id {
        pair.b_id
    } else {
        pair.a_id
    };
    tenant.core.supersede(&obsolete_id, &winner_id).await?;
    // The judge's explanation is carried through rather than dropped: it is the
    // only record of why this supersede was applied, and `set_pair_state`
    // writes `detail` unconditionally, so passing `None` would null it.
    tenant
        .core
        .store
        .set_pair_state(
            pid,
            crate::store::pairs::PairState::Dismissed,
            pair.detail.as_deref(),
            crate::store::pairs::DecidedBy::Operator,
        )
        .await?;
    Ok(Redirect::to(f.back.path()).into_response())
}

async fn deprecate_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    tenant.core.deprecate(&aid).await?;
    artifact_changed(&tenant, &headers, &aid, &p.terms, &back).await
}

async fn reactivate_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    tenant.core.reactivate(&aid).await?;
    // Whichever of the two hid it stamps; the other stamps nothing.
    for kind in [
        crate::store::actions::Kind::Discard,
        crate::store::actions::Kind::Reap,
    ] {
        tenant
            .core
            .store
            .undo_action_on(
                &aid,
                kind,
                crate::store::actions::UndoneBy::Operator,
                "reactivated on Insights",
            )
            .await?;
    }
    artifact_changed(&tenant, &headers, &aid, &p.terms, &back).await
}

async fn verify_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    tenant.core.verify(&aid).await?;
    artifact_changed(&tenant, &headers, &aid, &p.terms, &back).await
}

async fn undo_merge_ui(tenant: Tenant, Path(aid): Path<String>) -> UiResult<Response> {
    use crate::store::actions::UndoneBy;
    crate::jobs::merge::undo(&tenant.core, &aid, crate::store::pairs::DecidedBy::Operator).await?;
    tenant
        .core
        .store
        .undo_actions_under(&aid, UndoneBy::Operator, "undone on Insights")
        .await?;
    Ok(Redirect::to("/ui/insights").into_response())
}

/// Take a merge back: what it replaced returns, the merge is retired, and the
/// pairs behind it are dismissed so the sweep does not simply redo it.
/// Put the version a condensation retired back. One button per open
/// condensation on the artifact's page; the base's own undo is the same
/// method with `UndoneBy::Evidence`.
/// The path is the *condensation's* id and `artifact_changed` wants the
/// artifact's, so the action is read for its `subject_id` before it is undone
/// — after which the row still names the same artifact, but reading it first
/// keeps the failure ordinary: no such action is a `404` with nothing written.
async fn uncondense_ui(
    tenant: Tenant,
    headers: axum::http::HeaderMap,
    Path(aid): Path<String>,
    Query(p): Query<ArtifactViewParams>,
    Form(back): Form<ReturnTo>,
) -> UiResult<Response> {
    let artifact_id = tenant
        .core
        .store
        .action(&aid)
        .await?
        .ok_or(crate::error::Error::NotFound)?
        .subject_id;
    tenant
        .core
        .uncondense(&aid, crate::store::actions::UndoneBy::Operator)
        .await?;
    artifact_changed(&tenant, &headers, &artifact_id, &p.terms, &back).await
}

/// A pair waiting on a person.
pub struct PairRow {
    pub id: i64,
    pub percent: i64,
    pub a_id: String,
    pub a_title: String,
    pub b_id: String,
    pub b_title: String,
    /// Each side's opening words, said beside its title only when that title
    /// is shared with another row on the page. Three artifacts genuinely
    /// titled "LevelDB: Funktionsweise und forensische Analyse" turned one
    /// cluster of questions into what looked like one question asked three
    /// times. Same rule as `disambiguate_labels`, same reason.
    pub a_opening: String,
    pub b_opening: String,
    /// Enough of each side to decide by. The titles are links, but following
    /// one leaves the queue and comes back to a card whose other half you now
    /// have to remember — which is not a comparison, it is two readings with a
    /// navigation between them.
    pub a_excerpt: String,
    pub b_excerpt: String,
    pub detail: Option<String>,
    /// The stored `detail` is exactly `"link"` — the judge's duplicate
    /// hand-off (§7), a provenance marker, not prose. The row renders a
    /// sentence explaining that instead, and the percent is not shown as a
    /// measured similarity, because no cosine was ever computed for a pair
    /// found by co-retrieval.
    pub via_link: bool,
    pub contradiction: bool,
    /// Set when the judge named a direction with enough confidence to propose
    /// a supersede. A recommendation only: nothing here has hidden anything,
    /// and either side can still be kept.
    pub obsolete_title: Option<String>,
    /// Which side the judge's proposal amounts to keeping, so the row can
    /// accent that button. Both false when it made no proposal — every pair is
    /// still resolvable, just with nothing recommended.
    pub keeps_a: bool,
    pub keeps_b: bool,
    /// The judge found that neither side states anything. Accents "Discard
    /// both" the way `keeps_a` accents a Keep — a recommendation about which
    /// button to press, not a third thing to do.
    pub vacuous: bool,
    /// Every root of both members is `Captured`, so `insert_merged_artifact`
    /// will accept a merge over them.
    ///
    /// The same lineage check `jobs::dedupe` makes at admission, asked here so
    /// the card does not offer a button whose press can only come back a
    /// validation error. A passage is its own root, so a pair of passages
    /// fails it — which is the common case and exactly the one worth not
    /// offering.
    pub mergeable: bool,
    /// An operator has already pressed Synthese and the writing is queued. The
    /// row says so instead of offering the answers again.
    pub synthesis_asked: bool,
}

/// One decision, however many pairs it takes to state it.
pub struct PairCluster {
    pub pairs: Vec<PairRow>,
    /// How many distinct artifacts the cluster names, so the card can say what
    /// it is asking about before the rows do.
    pub members: usize,
}

/// The first `PAIR_LIMIT` pairs still waiting on a judgement, and how many more
/// there are behind them.
///
/// Used by Capture, which shows them because that is where the work arrives,
/// and by nothing else: Housekeeping is what is left over once the only part of
/// Ops that needs a person has moved to the page people actually open.
/// What the decide card calls one side of a pair, and the opening it prints
/// beside it.
///
/// The card names both sides in prose, in two Keep buttons and in the confirm
/// dialog, so an empty name is not an option here — a passage is named by how
/// its text opens (`row_label`). The opening then goes: it is the same words,
/// and `disambiguate_pair_titles` only ever adds it to tell two rows carrying
/// one name apart.
fn pair_side(c: &crate::store::artifacts::Chunk) -> (String, String) {
    let label = crate::web::ui::row_label(c);
    let opening = match label.named {
        true => crate::web::markdown::stand_in_title(&c.text, 40),
        false => String::new(),
    };
    (label.text, opening)
}

pub(crate) async fn pair_rows(tenant: &Tenant) -> Result<(Vec<PairRow>, i64)> {
    let mut waiting = 0i64;
    for state in PAIR_STATES {
        waiting += tenant.core.store.count_pairs_awaiting_review(state).await?;
    }

    let mut pairs = Vec::new();
    'fill: for state in PAIR_STATES {
        // Awaiting review, not merely in the state: every button these cards
        // carry supersedes, deprecates or merges, and all three refuse an
        // artifact that is not active. A pair one of those has already taken
        // out of results is work nobody can do, and offering it answered the
        // press with `cannot supersede: loser … is superseded`.
        for p in tenant
            .core
            .store
            .pairs_awaiting_review(state, PAIR_LIMIT as i64)
            .await?
        {
            let (Ok(a), Ok(b)) = (
                tenant.core.store.get_artifact(&p.a_id).await,
                tenant.core.store.get_artifact(&p.b_id).await,
            ) else {
                continue;
            };
            let (side_a, side_b) = (pair_side(&a), pair_side(&b));
            // The same name the card gives that side, not a second reading of
            // the same question: the judge's line said "Kapitel 3" about a
            // passage the two links beside it called by its opening.
            let obsolete_title = p.obsolete_id.as_deref().map(|id| match id == a.id {
                true => side_a.0.clone(),
                false => side_b.0.clone(),
            });
            // Keeping one side is superseding the other, so the judge naming
            // `a` obsolete is a recommendation to keep `b`.
            let keeps_a = p.obsolete_id.as_deref() == Some(b.id.as_str());
            let keeps_b = p.obsolete_id.as_deref() == Some(a.id.as_str());
            // A score of exactly zero is what "no cosine was ever measured"
            // looks like in the row: the link judge's `duplicate` verdict files
            // the pair with one (`src/jobs/associate.rs`), and the similarity
            // sweep — the only other producer of a pending pair — files the
            // cosine it found, which cleared `consolidate.review_min` to get
            // there (`src/jobs/relate.rs:68`).
            //
            // That gate is `>=` and `review_min` has no lower bound of its own
            // — only `auto_supersede > review_min` is enforced — so an operator
            // who sets it to zero could in principle file a pair measured at
            // exactly 0.0, and this would call it unmeasured. It takes an exact
            // float zero out of a real embedding to get there, which is why the
            // marker is left implicit; if that ever stops being true the fix is
            // an explicit `origin` column, not a smaller epsilon.
            //
            // Not `detail == "link"`, which is only the *initial* detail: the
            // dedupe judge's `set_pair_state` and `set_pair_superseded`
            // (`src/store/pairs.rs`) both write their own prose over that
            // field, so a marker read out of it survives only while the pair is
            // pending. The score is never rewritten.
            let via_link = p.score == 0.0;
            // The bare marker, on the other hand, *is* read out of `detail` —
            // it is the whole of that field only while the pair is pending, and
            // that is exactly when there is no judge's line to lose. Once one
            // has been written the prose is what the reader needs; the score
            // above still keeps the page from calling it a measurement.
            let detail = if p.detail.as_deref() == Some("link") {
                Some(
                    "Not found by similarity: these two kept being retrieved together, \
                     and the judge then found they say the same thing."
                        .to_string(),
                )
            } else {
                p.detail
            };
            // The lineage check `jobs::dedupe` makes before it calls the
            // model. Asked per row rather than once, because a pair names two
            // artifacts and each carries its own lineage.
            let member_ids = vec![p.a_id.clone(), p.b_id.clone()];
            let root_map = tenant.core.store.roots_of(&member_ids).await?;
            let all_roots: Vec<String> = root_map.values().flatten().cloned().collect();
            let mergeable = !all_roots.is_empty()
                && tenant
                    .core
                    .store
                    .artifacts_by_ids(&all_roots)
                    .await?
                    .iter()
                    .all(|r| r.provenance == crate::store::artifacts::Provenance::Captured);
            let synthesis_asked = p.synthesis_asked;
            pairs.push(PairRow {
                id: p.id,
                percent: (p.score * 100.0).round() as i64,
                a_title: side_a.0,
                b_title: side_b.0,
                // Kept whether or not it is shown; `disambiguate_pair_titles`
                // clears the ones the page does not need.
                a_opening: side_a.1,
                b_opening: side_b.1,
                a_excerpt: crate::web::markdown::snippet(&a.text, 400),
                b_excerpt: crate::web::markdown::snippet(&b.text, 400),
                a_id: p.a_id,
                b_id: p.b_id,
                detail,
                via_link,
                contradiction: state == crate::store::pairs::PairState::Contradiction,
                obsolete_title,
                keeps_a,
                keeps_b,
                vacuous: state == crate::store::pairs::PairState::Vacuous,
                mergeable,
                synthesis_asked,
            });
            if pairs.len() == PAIR_LIMIT {
                break 'fill;
            }
        }
    }

    // Counted under the listing's own rule, so a pair the queue will not show
    // is not announced as something waiting that never appears. The rows are
    // still skipped above for the case the count cannot see: an artifact
    // deleted between the two queries.
    let more = (waiting - pairs.len() as i64).max(0);
    disambiguate_pair_titles(&mut pairs);
    Ok((pairs, more))
}

/// Group the open pairs into the clusters they actually describe.
///
/// The same disjoint-set `jobs::consolidate` runs before it settles anything,
/// and for the same reason it gives: resolving pairs one at a time does not
/// work, and the way it fails is quiet. Here the failure is the operator's
/// rather than the base's — one artifact against three others arrived as three
/// separate questions, 90%, 90% and 88% alike, and answering one of them left
/// the other two on the page looking identical to the one just answered.
///
/// Order is the incoming order, which is `PAIR_STATES`' priority: the cluster
/// containing the most urgent pair leads, and within a cluster the rows keep
/// the order they were read in.
pub(crate) fn group_pairs(pairs: Vec<PairRow>) -> Vec<PairCluster> {
    let mut parent: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    fn find(parent: &mut std::collections::HashMap<String, String>, x: &str) -> String {
        let p = parent.get(x).cloned().unwrap_or_else(|| x.to_string());
        if p == x {
            return p;
        }
        let root = find(parent, &p);
        parent.insert(x.to_string(), root.clone());
        root
    }
    for r in &pairs {
        let (ra, rb) = (find(&mut parent, &r.a_id), find(&mut parent, &r.b_id));
        if ra != rb {
            parent.insert(ra, rb);
        }
    }

    let mut order: Vec<String> = Vec::new();
    let mut by_root: std::collections::HashMap<String, Vec<PairRow>> =
        std::collections::HashMap::new();
    for r in pairs {
        let root = find(&mut parent, &r.a_id);
        if !by_root.contains_key(&root) {
            order.push(root.clone());
        }
        by_root.entry(root).or_default().push(r);
    }

    order
        .into_iter()
        .filter_map(|root| {
            let pairs = by_root.remove(&root)?;
            let members: std::collections::HashSet<&str> = pairs
                .iter()
                .flat_map(|r| [r.a_id.as_str(), r.b_id.as_str()])
                .collect();
            Some(PairCluster {
                members: members.len(),
                pairs,
            })
        })
        .collect()
}

/// The same repair as `disambiguate_labels`, for the pair cards.
///
/// A pair names two artifacts, so a page of pairs has two columns of titles
/// that can collide, and on the deployment they did: three of five cards read
/// `… vs LevelDB: Funktionsweise und forensische Analyse` because three
/// distinct artifacts carried that one name. Each side keeps its opening words
/// only where its title is shared, for the reason the queue keeps them — a
/// suffix on a name that needs no suffix is noise.
fn disambiguate_pair_titles(rows: &mut [PairRow]) {
    // By distinct artifact, never by how often a title appears. One artifact
    // against three others — which is what a cluster looks like from here —
    // puts its name on three rows without anything colliding: it is the same
    // artifact each time, and a qualifier on it would say that three rows are
    // about different things when they are about one. A title collides when
    // two different ids carry it.
    let mut ids: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
        std::collections::HashMap::new();
    for r in rows.iter() {
        ids.entry(r.a_title.as_str())
            .or_default()
            .insert(r.a_id.as_str());
        ids.entry(r.b_title.as_str())
            .or_default()
            .insert(r.b_id.as_str());
    }
    let collides: std::collections::HashSet<String> = ids
        .into_iter()
        .filter(|(_, seen)| seen.len() > 1)
        .map(|(t, _)| t.to_string())
        .collect();
    for r in rows.iter_mut() {
        let a_collides = collides.contains(&r.a_title);
        let b_collides = collides.contains(&r.b_title);
        if !(a_collides && !r.a_opening.is_empty() && r.a_opening != r.a_title) {
            r.a_opening.clear();
        }
        if !(b_collides && !r.b_opening.is_empty() && r.b_opening != r.b_title) {
            r.b_opening.clear();
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui/ops/corpora/{id}/resolve", post(resolve_near_dupe_ui))
        .route("/ui/ops/artifacts/{id}/unsupersede", post(unsupersede_ui))
        .route("/ui/ops/artifacts/{id}/deprecate", post(deprecate_ui))
        .route("/ui/ops/artifacts/{id}/reactivate", post(reactivate_ui))
        .route("/ui/ops/merges/{id}/undo", post(undo_merge_ui))
        .route("/ui/ops/condensations/{id}/undo", post(uncondense_ui))
        .route("/ui/ops/artifacts/{id}/verify", post(verify_ui))
        .route("/ui/ops/pairs/{id}/dismiss", post(dismiss_pair_ui))
        .route("/ui/ops/pairs/{id}/discard", post(discard_pair_ui))
        .route(
            "/ui/ops/pairs/{id}/supersede",
            post(apply_pair_supersede_ui),
        )
        .route("/ui/ops/pairs/{id}/synthesize", post(ask_pair_synthesis_ui))
}

#[cfg(test)]
mod tests {
    /// Every button in the artifact pane that posts to `/ui/ops` must carry an
    /// `hx-post` and a `to`, or pressing it from a search result navigates the
    /// whole window away and the results being worked through are gone — the
    /// failure `ReturnTo` exists to prevent. "Restore the last version" was a
    /// bare `<form method="post">` for exactly as long as nothing checked, and
    /// `uncondense_ui` redirected to /ui/insights whatever page it was pressed
    /// from. Asserted over the template rather than on that one button, so the
    /// next button added is held to it too.
    #[test]
    fn every_ops_button_in_the_artifact_pane_returns_to_where_it_was_pressed() {
        let tpl = include_str!("templates/_artifact_detail.html");
        let mut checked = 0;
        for form in tpl.split("<form ").skip(1) {
            let head = &form[..form.find('>').expect("an opening form tag")];
            if !head.contains("/ui/ops/") {
                continue;
            }
            let body = &form[..form.find("</form>").expect("a closed form")];
            assert!(head.contains("hx-post="), "no hx-post: {head}");
            assert!(
                body.contains(r#"name="to""#),
                "nothing to return to: {head}"
            );
            checked += 1;
        }
        assert!(
            checked >= 5,
            "the pane's ops buttons went missing: {checked}"
        );
    }

    use super::*;
    use crate::web::test_support::{
        app_session_and_core, app_with_cookie, artifacts, body_of, form, get_body, row_on,
        session_with_an_artifact,
    };
    use askama::Template;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// The decide card names both sides in prose, in the two Keep buttons and
    /// in the confirm dialog. Emptied, the buttons read `Keep ""` and the two
    /// links have nothing to click — so a passage is named here by how its
    /// text opens, and the opening beside it goes, or the card says it twice.
    #[test]
    fn a_side_of_a_pair_that_has_no_name_is_named_by_its_opening() {
        let passage = crate::store::artifacts::Chunk {
            provenance: crate::store::artifacts::Provenance::Passage,
            title: Some("Kapitel 3".into()),
            text: "Der Vorgang setzt voraus, dass das Journal noch steht.".into(),
            ..crate::web::test_support::chunk_fixture(None, "")
        };
        let (title, opening) = pair_side(&passage);
        assert!(title.starts_with("Der Vorgang setzt voraus"), "{title:?}");
        assert!(!title.contains("Kapitel"), "{title:?}");
        assert!(
            opening.is_empty(),
            "the opening stood beside itself: {opening:?}"
        );

        let named = crate::store::artifacts::Chunk {
            title: Some("Wie ein Journal steht".into()),
            ..passage.clone()
        };
        let named = crate::store::artifacts::Chunk {
            provenance: crate::store::artifacts::Provenance::Captured,
            ..named
        };
        let (title, opening) = pair_side(&named);
        assert_eq!(title, "Wie ein Journal steht");
        assert!(
            !opening.is_empty(),
            "a named side still needs its opening to tell two of them apart"
        );
    }

    fn pair_row_fixture(a_id: &str, a_title: &str, a_opening: &str) -> PairRow {
        PairRow {
            id: 1,
            percent: 90,
            a_id: a_id.into(),
            a_title: a_title.into(),
            b_id: "b".into(),
            b_title: "SQLite-Datenbankeinstellungen und WAL".into(),
            a_opening: a_opening.into(),
            b_opening: "Einstellungen der SQLite-Datenbank".into(),
            a_excerpt: "Auto Vacuum werden freie Pages in der Free Page List verwaltet".into(),
            b_excerpt: "Einstellungen der SQLite-Datenbank koennen ueber Pragma".into(),
            detail: None,
            via_link: false,
            contradiction: true,
            obsolete_title: None,
            vacuous: false,
            keeps_a: false,
            keeps_b: false,
            mergeable: false,
            synthesis_asked: false,
        }
    }

    #[test]
    fn a_pair_card_carries_both_texts_to_read_in_place() {
        // The titles were links, so reading either side meant leaving the
        // queue and coming back to a card whose other half you now have to
        // remember.
        // `_decide.html` is only ever included, so it has no template struct
        // of its own; this is one, standing in for the page that includes it.
        #[derive(Template)]
        #[template(path = "_decide.html")]
        struct Decide {
            pairs: Vec<PairCluster>,
        }
        let html = askama::Template::render(&Decide {
            pairs: group_pairs(vec![pair_row_fixture("a1", "Auto Vacuum", "")]),
        })
        .unwrap();
        assert!(html.contains("<details"), "{html}");
        assert!(
            html.contains("Auto Vacuum werden freie Pages"),
            "the A side's text is not on the card: {html}"
        );
        assert!(
            html.contains("Einstellungen der SQLite-Datenbank koennen"),
            "the B side's text is not on the card: {html}"
        );
    }

    #[test]
    fn pairs_that_share_an_artifact_are_one_card() {
        // The deployment showed one artifact against three others as three
        // separate questions, 90%, 90% and 88% alike — the same decision
        // asked three times, where answering one did not retire the others.
        let p = |id: i64, a: &str, b: &str| PairRow {
            id,
            a_id: a.into(),
            b_id: b.into(),
            ..pair_row_fixture(a, "t", "")
        };
        let grouped = group_pairs(vec![
            p(1, "a", "b"),
            p(2, "a", "c"),
            p(3, "a", "d"),
            p(4, "x", "y"),
        ]);
        assert_eq!(grouped.len(), 2, "{} groups", grouped.len());
        assert_eq!(grouped[0].pairs.len(), 3);
        assert_eq!(grouped[0].members, 4, "one artifact against three others");
        assert_eq!(grouped[1].pairs.len(), 1);
        assert_eq!(grouped[1].members, 2);
    }

    #[test]
    fn a_chain_of_pairs_is_one_cluster_even_without_a_shared_artifact() {
        // a–b and b–c name no artifact in common, but resolving them
        // separately is what leaves A pointing at an artifact that is itself
        // hidden — the dead end `jobs::consolidate` documents.
        let p = |id: i64, a: &str, b: &str| PairRow {
            id,
            a_id: a.into(),
            b_id: b.into(),
            ..pair_row_fixture(a, "t", "")
        };
        let grouped = group_pairs(vec![p(1, "a", "b"), p(2, "b", "c")]);
        assert_eq!(grouped.len(), 1, "the chain was split");
        assert_eq!(grouped[0].members, 3);
    }

    #[test]
    fn pair_rows_sharing_a_title_are_disambiguated_too() {
        // Three artifacts on the deployment were titled "LevelDB:
        // Funktionsweise und forensische Analyse", so one cluster of
        // questions read as the same question asked three times.
        let mut rows = vec![
            // Two different artifacts that synthesis gave one name.
            pair_row_fixture(
                "a1",
                "LevelDB: Funktionsweise",
                "Der Aufbau der Datenlagerung",
            ),
            pair_row_fixture("a2", "LevelDB: Funktionsweise", "Die Extraktion der Keys"),
            pair_row_fixture(
                "a3",
                "Auto Vacuum und die Free Page List",
                "Freie Pages werden",
            ),
        ];
        disambiguate_pair_titles(&mut rows);
        assert!(!rows[0].a_opening.is_empty(), "nothing tells row 0 apart");
        assert_ne!(
            (&rows[0].a_title, &rows[0].a_opening),
            (&rows[1].a_title, &rows[1].a_opening),
            "still identical"
        );
        assert!(
            rows[2].a_opening.is_empty(),
            "a unique title needs no opening beside it: {:?}",
            rows[2].a_opening
        );
        assert!(
            rows[0].b_opening.is_empty(),
            "every row's B side is one artifact under one name — appearing three \
             times is a cluster, not a collision"
        );
    }

    #[tokio::test]
    async fn reactivating_a_discarded_artifact_stamps_its_row_as_undone_by_the_operator() {
        use crate::store::actions::{Kind, UndoneBy};
        let (app, cookie, core, aid) = session_with_an_artifact().await;
        core.deprecate_with(&aid, Some(row_on(&aid, Kind::Discard)))
            .await
            .unwrap();
        assert!(
            core.store
                .open_action_on(&aid, Kind::Discard)
                .await
                .unwrap()
                .is_some()
        );

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{aid}/reactivate"),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert!(
            res.status().is_redirection() || res.status().is_success(),
            "{}",
            res.status()
        );

        assert!(core.store.get_artifact(&aid).await.unwrap().in_results());
        assert!(
            core.store
                .open_action_on(&aid, Kind::Discard)
                .await
                .unwrap()
                .is_none()
        );
        let row = core.store.recent_actions(1).await.unwrap().remove(0);
        assert_eq!(row.undone_by, Some(UndoneBy::Operator));
    }

    #[tokio::test]
    async fn pressing_undo_on_a_merge_stamps_its_rows_as_taken_back_by_the_operator() {
        use crate::store::actions::{Kind, UndoneBy};
        let mut core = crate::core::test_support::test_core().await;
        // Through the press: a duplicate verdict is a proposal now and writes
        // nothing on its own, so the writer is what this test needs.
        core.pair_synthesizer = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec![
                r#"{"merged":{"title":"Mounting","text":"Mount the filesystem, or attach the volume, before writing.",
                          "category":"procedure","caveats":[]}}"#
                    .into(),
            ]),
        ));
        let ids = crate::jobs::consolidate::tests::seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()[0]
            .id;
        core.store.ask_pair_synthesis(pair).await.unwrap();
        crate::jobs::dedupe::run(&core, &pair.to_string())
            .await
            .unwrap();
        let merged = core.store.merged_artifacts(10).await.unwrap()[0].id.clone();
        crate::jobs::embed::run(&core, &merged).await.unwrap();
        assert_eq!(
            core.store
                .open_actions(&[Kind::Merge], 10)
                .await
                .unwrap()
                .len(),
            2
        );
        let (app, cookie) = app_with_cookie(core.clone()).await;

        app.oneshot(form(&format!("/ui/ops/merges/{merged}/undo"), &cookie, ""))
            .await
            .unwrap();

        assert!(
            core.store
                .open_actions(&[Kind::Merge], 10)
                .await
                .unwrap()
                .is_empty()
        );
        let rows = core.store.recent_actions(2).await.unwrap();
        assert!(rows.iter().all(|r| r.undone_by == Some(UndoneBy::Operator)));
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
    }

    #[tokio::test]
    async fn unsuperseding_stamps_the_supersede_row() {
        use crate::store::actions::{Kind, UndoneBy};
        let (app, cookie, core, aid) = session_with_an_artifact().await;
        let out = core
            .ingest_capture(crate::core::ingest::Capture::new(
                "The pool holds sixteen connections, and the timeout is 30s.",
                "ui",
            ))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        let winner = core
            .store
            .artifacts_for_corpus(&out.id)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.in_results())
            .expect("a live artifact")
            .id;
        core.supersede_with(
            &aid,
            &winner,
            Some(crate::store::actions::NewAction {
                survivor_id: Some(winner.clone()),
                ..row_on(&aid, Kind::Supersede)
            }),
        )
        .await
        .unwrap();

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{aid}/unsupersede"),
                &cookie,
                "",
            ))
            .await
            .unwrap();

        assert!(
            core.store
                .get_artifact(&aid)
                .await
                .unwrap()
                .superseded_by
                .is_none()
        );
        let row = core.store.recent_actions(1).await.unwrap().remove(0);
        assert_eq!(row.kind, Kind::Supersede);
        assert_eq!(row.undone_by, Some(UndoneBy::Operator));
    }

    #[tokio::test]
    async fn ops_lists_a_superseded_artifact_and_can_undo_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["the loser", "the keeper"]).await;
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/insights")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = body_of(res).await;
        assert!(
            html.contains("the loser") && html.contains("the keeper"),
            "the superseded artifact is not listed"
        );

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/artifacts/{}/unsupersede", ids[0]),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "undo did not clear the flag"
        );
    }

    #[tokio::test]
    async fn a_contradiction_the_judge_could_not_call_is_still_resolvable() {
        // The dead end this fixes: the judge finds two artifacts stating a
        // detail differently but names no winner, so `obsolete_id` is NULL. The
        // row then offered nothing but Dismiss — an operator who could see which
        // one was right had no way to say so, and clearing the queue meant
        // declaring the disagreement uninteresting and leaving both in results.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["left one", "right one"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        core.store
            .set_pair_state(
                pair.id,
                crate::store::pairs::PairState::Contradiction,
                Some("they disagree about the tag"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();
        assert!(
            core.store
                .get_pair(pair.id)
                .await
                .unwrap()
                .obsolete_id
                .is_none(),
            "this test is only meaningful with no judge proposal to fall back on"
        );

        // Keep the first; the second is the one that gets hidden.
        app.clone()
            .oneshot(form(
                &format!("/ui/ops/pairs/{}/supersede", pair.id),
                &cookie,
                &format!("keep={}", pair.a_id),
            ))
            .await
            .unwrap();

        let kept = core.store.get_artifact(&pair.a_id).await.unwrap();
        let hidden = core.store.get_artifact(&pair.b_id).await.unwrap();
        assert_eq!(kept.status, crate::store::artifacts::ArtifactStatus::Active);
        assert_eq!(
            hidden.status,
            crate::store::artifacts::ArtifactStatus::Superseded
        );
        assert_eq!(hidden.superseded_by.as_deref(), Some(pair.a_id.as_str()));
    }

    #[tokio::test]
    async fn keeping_an_artifact_from_outside_the_pair_is_refused() {
        // `keep` is a form field, so it is user input. Superseding whatever id
        // arrives would hide an artifact that has nothing to do with the row
        // that was pressed.
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["left one", "right one", "unrelated"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/pairs/{}/supersede", pair.id),
                &cookie,
                &format!("keep={}", ids[2]),
            ))
            .await
            .unwrap();

        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().status,
                crate::store::artifacts::ArtifactStatus::Active,
                "an artifact outside the pair was touched"
            );
        }
    }

    /// Every button on a pair card ends in a call that refuses an artifact
    /// which is not active, so a pair naming one is work nobody can do. It was
    /// still offered, and the press came back with a validation error.
    #[tokio::test]
    async fn a_pair_whose_member_left_results_is_not_offered_for_review() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["the timeout is 30 seconds", "the timeout is 90"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        core.store
            .set_pair_state(
                pair.id,
                crate::store::pairs::PairState::Contradiction,
                Some("30 seconds vs 90"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();
        core.deprecate(&ids[1]).await.unwrap();

        let html = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            !html.contains(&format!("/ui/ops/pairs/{}/supersede", pair.id)),
            "the queue offered a pair whose member is out of results: {html}"
        );
        assert!(
            !html.contains("more waiting"),
            "the queue counted a pair it will never show: {html}"
        );
    }

    #[tokio::test]
    async fn capture_lists_a_pending_pair_and_can_dismiss_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["left one", "right one"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);

        // On Capture, not on Housekeeping: this is the one part of Ops that
        // needs a person, so it belongs where the work arrives.
        let html = get_body(&app, &cookie, "/ui/insights").await;
        assert!(html.contains("left one") && html.contains("right one"));
        assert!(
            html.contains("Keep “left one”"),
            "each button names the artifact it keeps"
        );

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/pairs/{}/dismiss", pair.id),
                &cookie,
                "",
            ))
            .await
            .unwrap();
        assert!(
            core.store
                .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// `insert_merged_artifact` refuses a lineage that names anything but
    /// captured roots, so offering the button over passages is offering a press
    /// that can only answer with a validation error. A passage is its own root,
    /// which is what makes this the common case rather than an edge one.
    #[tokio::test]
    async fn the_synthesize_button_is_offered_only_where_a_merge_can_be_written() {
        use crate::store::artifacts::{NewArtifact, Provenance};
        let (app, cookie, core) = app_session_and_core().await;

        // Two captured artifacts: a merge over them is allowed.
        let captured = artifacts(&core, &["clinic hours", "clinic services"]).await;
        core.store
            .record_pair(&captured[0], &captured[1], 0.9)
            .await
            .unwrap();
        let html = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            html.contains("Synthese"),
            "a captured pair can become one artifact"
        );

        // Two passages: stored source text, which a merge may not rewrite.
        let src = core.store.insert_corpus("y", "web", None).await.unwrap();
        let raw: Vec<NewArtifact> = ["verbatim left", "verbatim right"]
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: (*t).to_string(),
                title: Some((*t).to_string()),
                ..Default::default()
            })
            .collect();
        let passages: Vec<String> = core
            .store
            .insert_artifacts_with_provenance(&src.id, &raw, Provenance::Passage)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();

        core.store
            .record_pair(&passages[0], &passages[1], 0.9)
            .await
            .unwrap();

        // Both pairs are on the page now. Exactly one of them offers the
        // button, and it is not the one made of stored source text.
        let html = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            html.contains("verbatim left"),
            "the passage pair is on the queue"
        );
        assert_eq!(
            html.matches("/synthesize").count(),
            1,
            "only the captured pair offers a synthesis"
        );
    }

    /// The press records a judgement and arms a unit. It writes no artifact and
    /// calls no model: no route under `src/web` calls one, and a handler that
    /// blocked on a judge would hold the request open for as long as the
    /// endpoint felt like taking.
    #[tokio::test]
    async fn pressing_synthesize_records_the_ask_and_writes_nothing() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["clinic hours", "clinic services"]).await;
        core.store.record_pair(&ids[0], &ids[1], 0.9).await.unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/pairs/{}/synthesize", pair.id),
                &cookie,
                "",
            ))
            .await
            .unwrap();

        let after = core.store.get_pair(pair.id).await.unwrap();
        assert!(after.synthesis_asked, "the ask is recorded");
        assert!(after.merged_into.is_none(), "the route writes no merge");
        for id in &ids {
            assert!(
                core.store.get_artifact(id).await.unwrap().in_results(),
                "and hides neither side"
            );
        }

        // The card stops offering the answers and says what is pending.
        let html = get_body(&app, &cookie, "/ui/insights").await;
        assert!(html.contains("A synthesis was asked for"));
    }

    /// A verdict is a recommendation on the card, never an action taken. So
    /// the pair has to reach the queue at all, and it has to say which of the
    /// three buttons the judge would press.
    #[tokio::test]
    async fn a_pair_the_judge_found_empty_recommends_discarding_both() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["notes/a.md", "notes/b.md"]).await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.99)
            .await
            .unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        core.store
            .set_pair_state(
                pair.id,
                crate::store::pairs::PairState::Vacuous,
                Some("each body is its own file path"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();

        let html = get_body(&app, &cookie, "/ui/insights").await;
        assert!(
            html.contains(&format!("/ui/ops/pairs/{}/discard", pair.id)),
            "a pair filed vacuous never reached the queue: {html}"
        );
        assert!(
            html.contains("neither of these says anything"),
            "the card asks the wrong question about this pair: {html}"
        );
    }

    /// The third answer a pair can have. Keeping one side is wrong when neither
    /// side is worth keeping, and Dismiss leaves both in results — so a pair of
    /// artifacts that say nothing had no way out of the queue that removed them.
    #[tokio::test]
    async fn discarding_a_pair_retires_both_sides() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["notes/a.md", "notes/b.md"]).await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.99)
            .await
            .unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);

        app.clone()
            .oneshot(form(
                &format!("/ui/ops/pairs/{}/discard", pair.id),
                &cookie,
                "",
            ))
            .await
            .unwrap();

        for id in &ids {
            assert!(
                !core.store.get_artifact(id).await.unwrap().in_results(),
                "discard left {id} in results"
            );
        }
        assert_eq!(
            core.store.get_pair(pair.id).await.unwrap().state,
            crate::store::pairs::PairState::Dismissed,
            "the pair is answered, so it must leave the queue"
        );
    }

    /// A cluster is answered one card at a time, and the sides do not wait
    /// their turn: applying a supersede on one row hides an artifact that
    /// another row still names, and nothing filters that row off the page.
    /// `core.deprecate` refuses an artifact already hidden that way, so the
    /// press used to retire the first side, fail on the second, and leave the
    /// pair open with half of it gone — and open is not answerable here, since
    /// the next press fails at exactly the same place.
    #[tokio::test]
    async fn discarding_a_pair_whose_other_side_is_already_hidden_still_answers_it() {
        let (app, cookie, core) = app_session_and_core().await;
        let ids = artifacts(&core, &["notes/a.md", "notes/b.md", "notes/c.md"]).await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.99)
            .await
            .unwrap();
        let pair = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        // The row alone, not `Core::supersede`: that path now moves the open
        // pairs of the artifact it hides onto the winner, so the card under
        // test would name two live artifacts instead. What this pins is the
        // press holding a card that still names a hidden side — the state the
        // sweep repairs after a crash between those two writes
        // (`jobs::consolidate::follow_supersessions`), and the window between a
        // supersession and the page being reloaded.
        core.store
            .set_superseded_by(&ids[1], Some(&ids[2]))
            .await
            .unwrap();

        let res = app
            .clone()
            .oneshot(form(
                &format!("/ui/ops/pairs/{}/discard", pair.id),
                &cookie,
                "",
            ))
            .await
            .unwrap();

        assert!(
            res.status().is_success() || res.status().is_redirection(),
            "the button reported a failure: {:?}",
            res.status()
        );
        for id in &ids[..2] {
            assert!(
                !core.store.get_artifact(id).await.unwrap().in_results(),
                "discard left {id} in results"
            );
        }
        assert_eq!(
            core.store.get_pair(pair.id).await.unwrap().state,
            crate::store::pairs::PairState::Dismissed,
            "the pair is answered, so it must leave the queue"
        );
    }
}
