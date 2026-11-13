//! Decide in the workspace: a decision as bars and a standing, the notes it rests on in the rail.

use askama::Template;
use axum::extract::{Form, Path, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::core::decide::standing::pick;
use crate::core::decide::verdict::Correct;
use crate::core::decide::wire::{DecideBody, stored_answer, stored_questions};
use crate::core::decide::{
    DecideEvent, Decision, Evidence, PassageRead, Standing, Verdict, normalised_questions,
};
use crate::error::{Error, Result};
use crate::infer::decide::{Answer, Kind, Question};
use crate::store::asks::AskVerdict;
use crate::store::decisions::StoredDecision;
use crate::store::feedback::Door;
use crate::tenants::Tenant;
use crate::web::markdown::stand_in_title;
use crate::web::state::AppState;
use crate::web::ui_error::UiResult;
use crate::web::workspace::verdict_label;

pub struct Bar {
    pub label: String,
    pub weight: u8,
    pub prior: u8,
    pub picked: bool,
}

impl Bar {
    /// The marker is one unit wide, so a prior of 100 sits at the track's end, not past it.
    pub fn prior_x(&self) -> u8 {
        self.prior.min(99)
    }
}

/// In `pick`'s order.
fn labels(q: &Question) -> Vec<String> {
    match &q.kind {
        Kind::Choice { options } => options.iter().map(|(n, _)| n.clone()).collect(),
        Kind::Score { levels } => levels.clone(),
        Kind::Noul => vec!["yes".into(), "no".into()],
    }
}

fn probabilities(a: &Answer) -> Vec<f64> {
    match a {
        Answer::Choice { probabilities, .. } | Answer::Score { probabilities, .. } => {
            probabilities.clone()
        }
        Answer::Noul { yes } => vec![*yes, 1.0 - yes],
    }
}

fn weight(p: Option<&f64>) -> u8 {
    (p.copied().unwrap_or(0.0) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8
}

/// One per option or level; a noul gives "yes" and "no". Weights are (p * 100).round().
pub fn bars(q: &Question, joint: &Answer, prior: &Answer) -> Vec<Bar> {
    let (j, p) = (probabilities(joint), probabilities(prior));
    let picked = pick(q, joint);
    labels(q)
        .into_iter()
        .enumerate()
        .map(|(i, label)| Bar {
            label,
            weight: weight(j.get(i)),
            prior: weight(p.get(i)),
            picked: i == picked,
        })
        .collect()
}

/// What a passage's own answer says, as a label: the option, the level, "yes" or "no".
pub fn pick_label(q: &Question, a: &Answer) -> String {
    label(q, pick(q, a)).unwrap_or_default()
}

fn label(q: &Question, i: usize) -> Option<String> {
    labels(q).into_iter().nth(i)
}

pub fn standing_copy(s: Standing) -> &'static str {
    match s {
        Standing::Settled => "Settled.",
        Standing::Leaning => "Leaning.",
        Standing::Split => "Split.",
        Standing::NotHeld => "",
    }
}

fn render(t: impl Template) -> Result<String> {
    t.render().map_err(|e| Error::Internal(e.to_string()))
}

fn joint(d: &Decision) -> Option<(&Question, &Answer)> {
    let q = d.questions.first()?;
    let (_, v) = d.answers.as_ref()?.first()?;
    Some((q, &v.answer))
}

#[derive(Template)]
#[template(path = "_decision.html")]
struct DecisionTemplate<'a> {
    question: &'a str,
    /// Empty when not held: the notice stands in their place.
    bars: Vec<Bar>,
    standing: &'static str,
    verdict_bar: String,
}

pub fn decision_fragment(d: &Decision, verdict: Option<&str>) -> Result<String> {
    let bars = match (joint(d), d.prior.first()) {
        (Some((q, j)), Some((_, p))) => bars(q, j, p),
        _ => vec![],
    };
    let standing = d.standing.first().map_or("", |(_, s)| standing_copy(*s));
    let verdict_bar = match d.event_id {
        Some(_) => verdict_fragment(&verdict_bar(d, verdict, None))?,
        None => String::new(),
    };
    render(DecisionTemplate {
        question: &d.question,
        bars,
        standing,
        verdict_bar,
    })
}

/// One rail row; `group` is set on the first row under a split's heading.
struct Row {
    n: usize,
    artifact_id: String,
    title: String,
    /// `None`: it does not bear.
    pick: Option<String>,
    against: bool,
    group: Option<String>,
}

#[derive(Template)]
#[template(path = "_decide_rail.html")]
struct RailTemplate {
    rows: Vec<Row>,
    /// Passages read that did not bear.
    rest: usize,
}

#[derive(Template)]
#[template(path = "_decide_rail_row.html")]
struct ReadingRowTemplate {
    r: Row,
}

fn own(q: &Question, read: &[(String, Answer)]) -> Option<usize> {
    read.iter()
        .find(|(k, _)| *k == q.key)
        .map(|(_, a)| pick(q, a))
}

fn evidence_row(
    e: &Evidence,
    q: Option<&Question>,
    verdict: Option<usize>,
) -> (Option<usize>, Row) {
    let mine = q.and_then(|q| own(q, &e.read));
    let title = e
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| stand_in_title(&e.text, 60));
    let row = Row {
        n: e.n,
        artifact_id: e.artifact_id.clone(),
        title,
        pick: q.zip(mine).and_then(|(q, i)| label(q, i)),
        against: verdict.is_some() && mine.is_some() && mine != verdict,
        group: None,
    };
    (mine, row)
}

pub fn rail_fragment(d: &Decision) -> Result<String> {
    let q = d.questions.first();
    let verdict = joint(d).map(|(q, a)| pick(q, a));
    let mut rows: Vec<(Option<usize>, Row)> = d
        .evidence
        .iter()
        .map(|e| evidence_row(e, q, verdict))
        .collect();
    if d.standing.first().map(|(_, s)| *s) == Some(Standing::Split) {
        // Stable, so each side keeps the evidence's own order.
        rows.sort_by_key(|(i, _)| *i);
        let mut last = None;
        for (i, r) in rows.iter_mut() {
            if *i != last {
                r.group = r.pick.clone();
                last = *i;
            }
        }
    }
    render(RailTemplate {
        rest: d.read.saturating_sub(d.evidence.len()),
        rows: rows.into_iter().map(|(_, r)| r).collect(),
    })
}

/// "Decided from 6 of 8"
pub fn rail_head(d: &Decision) -> String {
    if d.not_held() {
        // The rest line under the rows says how many did not bear.
        "Nothing decided".to_string()
    } else {
        format!("Decided from {} of {}", d.evidence.len(), d.read)
    }
}

pub fn reading_row(p: &PassageRead, q: &Question) -> Result<String> {
    render(ReadingRowTemplate {
        r: Row {
            n: p.rank,
            artifact_id: p.artifact_id.clone(),
            title: p.title.clone().unwrap_or_default(),
            pick: own(q, &p.read)
                .filter(|_| p.bearing)
                .and_then(|i| label(q, i)),
            against: false,
            group: None,
        },
    })
}

/// What the bar needs: the decision's first question, whether it was held, the joint pick, and what is stored.
#[derive(Template)]
#[template(path = "_decide_verdict.html")]
pub struct VerdictBar {
    pub event_id: String,
    pub held: bool,
    pub verdict: Option<String>,
    pub correct: Option<String>,
    pub remaining: Vec<(String, String)>,
}

impl VerdictBar {
    /// JSON in a single-quoted attribute: `'`, `<` and `&` become JSON escapes, so nothing needs HTML escaping.
    fn vals(&self, correct: &str) -> String {
        let value = serde_json::Value::from(correct).to_string();
        format!(r#"{{"verdict":"wrong","correct":{value}}}"#)
            .replace('\'', "\\u0027")
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026")
    }
}

/// As the verdict route takes it: an option name, a level index, or yes / no.
fn post_value(q: &Question, i: usize, label: &str) -> String {
    match q.kind {
        Kind::Score { .. } => i.to_string(),
        _ => label.to_string(),
    }
}

/// The stored `{key: answer}` as its label; a level is stored by index.
fn correct_label(q: &Question, stored: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(stored).ok()?;
    match v.get(&q.key)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => label(q, usize::try_from(n.as_u64()?).ok()?),
        _ => None,
    }
}

/// `correct` is the stored JSON, or "-" for "Not sure", which asks nothing more.
pub fn verdict_bar(d: &Decision, verdict: Option<&str>, correct: Option<&str>) -> VerdictBar {
    // After Right the right answer is the verdict's own pick, already in the bars.
    let named = correct
        .filter(|c| *c != "-" && verdict == Some("wrong"))
        .zip(d.questions.first())
        .and_then(|(c, q)| correct_label(q, c));
    let remaining = match joint(d) {
        Some((q, j)) if verdict == Some("wrong") && correct.is_none() && labels(q).len() >= 3 => {
            let own = pick(q, j);
            labels(q)
                .into_iter()
                .enumerate()
                .filter(|(i, _)| *i != own)
                .map(|(i, l)| (post_value(q, i, &l), l))
                .collect()
        }
        _ => vec![],
    };
    VerdictBar {
        event_id: d.event_id.clone().unwrap_or_default(),
        held: !d.not_held(),
        verdict: verdict.map(str::to_string),
        correct: named,
        remaining,
    }
}

pub fn verdict_fragment(bar: &VerdictBar) -> Result<String> {
    bar.render().map_err(|e| Error::Internal(e.to_string()))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        // The POST parks the question and returns an id; an `EventSource` spends it.
        .route("/ui/decide", post(decide_submit))
        .route("/ui/decide/{id}/stream", get(decide_stream))
        .route("/ui/decide/{id}/verdict", post(decide_verdict))
}

/// `/api/v1/decide`'s body; the web asks one question.
async fn decide_submit(
    State(st): State<AppState>,
    tenant: Tenant,
    Json(body): Json<DecideBody>,
) -> UiResult<Response> {
    let req = body.into_request()?;
    if req.questions.len() > 1 {
        return Err(
            Error::Validation("decide: the web asks one question at a time.".into()).into(),
        );
    }
    // Refused before anything is parked.
    normalised_questions(&req)?;
    let id = st.decide_handoff_park(req, &tenant.user.subject);
    Ok(Json(serde_json::json!({ "id": id })).into_response())
}

/// As `ask_stream`: unknown, spent, expired or another subject's id is one answer.
async fn decide_stream(
    State(st): State<AppState>,
    tenant: Tenant,
    Path(handoff): Path<String>,
) -> UiResult<Response> {
    use tokio_stream::StreamExt as _;

    let req = st
        .decide_handoff_take(&handoff, &tenant.user.subject)
        .ok_or(Error::NotFound)?;
    // As the pipeline normalises them, so a row names the options `done` names.
    let q = normalised_questions(&req)?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Internal("a decision without a question".into()))?;
    let core = tenant.core.clone();
    let origin = Door::Ui.by(tenant.user.subject);
    let events = async_stream::stream! {
        let s = core.decide_events(&req, origin);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            // Terminal: the `try_stream!` producer ends at its first error.
            match ev.and_then(|e| sse_event(e, &q)) {
                Ok(e) => yield Ok::<_, Error>(e),
                // An error ends the stream, a render failure too.
                Err(e) => {
                    yield Ok(error_event(&e));
                    break;
                }
            }
        }
    };
    // Kept alive: sixteen reads on a slow model outlast a proxy's idle timeout.
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// JSON for every payload but `error`: SSE frames data by line.
fn sse_event(ev: DecideEvent, q: &Question) -> Result<SseEvent> {
    let (name, data) = match ev {
        DecideEvent::Retrieved { passages } => {
            ("retrieved", serde_json::json!({ "passages": passages }))
        }
        DecideEvent::Read(r) => (
            "read",
            serde_json::json!({
                "rank": r.rank,
                "of": r.of,
                "bearing": r.bearing,
                "row": reading_row(&r, q)?,
            }),
        ),
        DecideEvent::Done(d) => (
            "done",
            serde_json::json!({
                "event_id": d.event_id,
                "html": decision_fragment(&d, None)?,
                "rail": rail_fragment(&d)?,
                "rail_head": rail_head(&d),
            }),
        ),
    };
    Ok(SseEvent::default().event(name).data(data.to_string()))
}

fn error_event(e: &Error) -> SseEvent {
    if e.status().is_server_error() {
        tracing::error!(error = %e, "decide stream failed");
    }
    SseEvent::default().event("error").data(e.client_message())
}

#[derive(serde::Deserialize)]
struct VerdictForm {
    verdict: String,
    /// With `wrong`: an option name, a level index, `yes`, `no`, or `-` for not sure.
    #[serde(default)]
    correct: Option<String>,
}

fn correction(q: &Question, c: &str) -> Result<Correct> {
    Ok(match (&q.kind, c) {
        (Kind::Score { .. }, _) => {
            Correct::Level(c.parse().map_err(|_| {
                Error::Validation("decide: a level is corrected by its index.".into())
            })?)
        }
        (_, "yes") => Correct::Yes,
        (_, "no") => Correct::No,
        _ => Correct::Option(c.to_string()),
    })
}

/// As much of the decision as the bar reads, from what was stored.
fn recorded(s: &StoredDecision) -> Result<Decision> {
    let questions = stored_questions(&s.questions)?;
    let answers = match (&s.answers, questions.first()) {
        (Some(a), Some(q)) if !s.not_held => stored_answer(q, a).map(|answer| {
            vec![(
                q.key.clone(),
                Verdict {
                    answer,
                    swapped_agree: None,
                },
            )]
        }),
        _ => None,
    };
    Ok(Decision {
        question: s.question.clone(),
        questions,
        answers,
        prior: vec![],
        standing: vec![],
        evidence: vec![],
        read: 0,
        dropped: 0,
        model: String::new(),
        event_id: Some(s.id.clone()),
        state: None,
        reads: vec![],
    })
}

/// The rules are `Core::judge_decision`'s; the answer is the re-rendered bar.
async fn decide_verdict(
    tenant: Tenant,
    Path(id): Path<String>,
    Form(f): Form<VerdictForm>,
) -> UiResult<Response> {
    let stored = tenant
        .core
        .store
        .decision(&id)
        .await?
        .ok_or(Error::NotFound)?;
    let d = recorded(&stored)?;
    let judged = match f.verdict.as_str() {
        "none" => tenant.core.unjudge_decision(&id).await?,
        v => {
            let verdict = AskVerdict::parse(v)
                .ok_or_else(|| Error::Validation(format!("unknown verdict {v}")))?;
            let correct = match (verdict, f.correct.as_deref(), d.questions.first()) {
                (AskVerdict::Wrong, Some(c), Some(q)) if c != "-" => Some(correction(q, c)?),
                _ => None,
            };
            tenant.core.judge_decision(&id, verdict, correct).await?
        }
    };
    let verdict = judged.verdict.map(verdict_label);
    // "Not sure" stores the verdict alone; the bar does not ask again.
    let correct = judged
        .correct
        .or_else(|| (f.correct.as_deref() == Some("-")).then(|| "-".to_string()));
    let mut bar = verdict_bar(&d, verdict.as_deref(), correct.as_deref());
    bar.held = !stored.not_held;
    Ok(Html(verdict_fragment(&bar)?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::decide::{Decision, Evidence, PassageRead, Standing, Verdict};
    use crate::infer::decide::{Answer, Kind, Question};

    fn choice(names: &[&str]) -> Question {
        Question {
            key: "verdict".into(),
            kind: Kind::Choice {
                options: names
                    .iter()
                    .map(|n| (n.to_string(), String::new()))
                    .collect(),
            },
            instructions: "Keep the embedder?".into(),
        }
    }

    fn scale(levels: &[&str]) -> Question {
        Question {
            key: "verdict".into(),
            kind: Kind::Score {
                levels: levels.iter().map(|l| l.to_string()).collect(),
            },
            instructions: "How soon?".into(),
        }
    }

    fn chose(q: &Question, p: &[f64]) -> Answer {
        let Kind::Choice { options } = &q.kind else {
            unreachable!()
        };
        let i = p
            .iter()
            .enumerate()
            .fold(0, |b, (i, &v)| if v > p[b] { i } else { b });
        Answer::Choice {
            pick: options[i].0.clone(),
            probabilities: p.to_vec(),
            confidence: p[i],
        }
    }

    fn scored(p: &[f64]) -> Answer {
        Answer::Score {
            score: 0.0,
            probabilities: p.to_vec(),
            confidence: 0.5,
        }
    }

    fn evidence(n: usize, title: Option<&str>, read: Answer) -> Evidence {
        Evidence {
            n,
            artifact_id: format!("A{n}"),
            title: title.map(str::to_string),
            text: format!("Passage {n} about the embedder."),
            bears: 1.0 - n as f64 * 0.05,
            joint: true,
            read: vec![("verdict".into(), read)],
        }
    }

    fn decision(q: Question, joint: Option<Answer>, prior: Answer, ev: Vec<Evidence>) -> Decision {
        let standing = if joint.is_some() {
            Standing::Settled
        } else {
            Standing::NotHeld
        };
        Decision {
            question: "Should I keep the bge-m3 embedder?".into(),
            questions: vec![q],
            answers: joint.map(|a| {
                vec![(
                    "verdict".to_string(),
                    Verdict {
                        answer: a,
                        swapped_agree: Some(true),
                    },
                )]
            }),
            prior: vec![("verdict".into(), prior)],
            standing: vec![("verdict".into(), standing)],
            evidence: ev,
            read: 8,
            dropped: 0,
            model: "rule".into(),
            event_id: Some("01EV".into()),
            state: None,
            reads: vec![],
        }
    }

    /// The spec's example: keep 81 / switch 19, prior 55 / 45, settled, six notes of which the third reads against.
    fn held() -> Decision {
        let q = choice(&["keep", "switch to gemma"]);
        let keep = chose(&q, &[0.9, 0.1]);
        let switch = chose(&q, &[0.2, 0.8]);
        let ev = (1..=6)
            .map(|n| {
                let a = if n == 3 { switch.clone() } else { keep.clone() };
                let title = (n != 5).then(|| format!("Note {n}"));
                evidence(n, title.as_deref(), a)
            })
            .collect();
        decision(
            q.clone(),
            Some(chose(&q, &[0.81, 0.19])),
            chose(&q, &[0.55, 0.45]),
            ev,
        )
    }

    fn not_held() -> Decision {
        let q = choice(&["keep", "switch to gemma"]);
        decision(q.clone(), None, chose(&q, &[0.55, 0.45]), vec![])
    }

    fn three_levels() -> Decision {
        let q = scale(&["can wait", "this month", "now"]);
        decision(
            q,
            Some(scored(&[0.1, 0.7, 0.2])),
            scored(&[0.3, 0.4, 0.3]),
            vec![],
        )
    }

    fn as_tuples(b: &[Bar]) -> Vec<(&str, u8, u8, bool)> {
        b.iter()
            .map(|b| (b.label.as_str(), b.weight, b.prior, b.picked))
            .collect()
    }

    #[test]
    fn bars_round_to_whole_weights() {
        let d = held();
        let b = bars(
            &d.questions[0],
            &d.answers.as_ref().unwrap()[0].1.answer,
            &d.prior[0].1,
        );
        assert_eq!(
            as_tuples(&b),
            [("keep", 81, 55, true), ("switch to gemma", 19, 45, false)]
        );
    }

    #[test]
    fn a_noul_draws_yes_and_no() {
        let q = Question::noul("verdict", "Keep?");
        let b = bars(&q, &Answer::Noul { yes: 0.9 }, &Answer::Noul { yes: 0.6 });
        assert_eq!(
            as_tuples(&b),
            [("yes", 90, 60, true), ("no", 10, 40, false)]
        );
    }

    #[test]
    fn a_score_draws_a_bar_per_level() {
        let q = scale(&["can wait", "this month", "now"]);
        let b = bars(&q, &scored(&[0.1, 0.7, 0.2]), &scored(&[0.3, 0.4, 0.3]));
        assert_eq!(
            as_tuples(&b),
            [
                ("can wait", 10, 30, false),
                ("this month", 70, 40, true),
                ("now", 20, 30, false)
            ]
        );
    }

    #[test]
    fn the_result_shows_weights_standing_and_no_percent_sign() {
        let h = decision_fragment(&held(), None).unwrap();
        assert!(h.contains(r#"<section class="answer decision">"#));
        assert!(h.contains("Should I keep the bge-m3 embedder?"));
        assert!(h.contains(r#"<rect class="bar-fill" width="81""#));
        assert!(h.contains(r#"<rect class="bar-prior" x="55""#));
        assert!(h.contains("keep: 81 of 100, 55 without notes"));
        assert!(h.contains("Settled."));
        assert!(!h.contains('%') && !h.to_lowercase().contains("likely"));
        assert_eq!(h.matches("without notes</p>").count(), 1, "labelled once");
        let legend = &h[h.find("bar-legend").unwrap()..];
        assert!(
            legend.contains(r#"<rect class="bar-prior""#),
            "the legend shows the mark"
        );
        assert!(!h.contains('▲'));
        assert!(h.contains(r#"hx-post="/ui/decide/01EV/verdict""#));
        assert!(!h.contains("Keep"));
    }

    #[test]
    fn a_decision_the_base_does_not_hold_shows_the_notice_and_no_bars() {
        let h = decision_fragment(&not_held(), None).unwrap();
        assert!(h.contains(
            r#"<span class="oc-notice__text">Nothing here: the base does not hold this</span>"#
        ));
        assert!(!h.contains("bar-track") && !h.contains("without notes"));
        assert!(h.contains(r#"id="decide-verdict""#));
    }

    #[test]
    fn an_unrecorded_decision_has_no_buttons() {
        let mut d = held();
        d.event_id = None;
        let h = decision_fragment(&d, None).unwrap();
        assert!(!h.contains("hx-post") && !h.contains("decide-verdict"));
    }

    #[test]
    fn the_rail_names_each_passages_pick_and_marks_the_ones_against() {
        let d = held();
        let r = rail_fragment(&d).unwrap();
        assert_eq!(r.matches(r#"class="rail-row""#).count(), 6);
        assert_eq!(r.matches(">against<").count(), 1);
        let third = &r[r.find("[3]").unwrap()..r.find("[4]").unwrap()];
        assert!(third.contains(">against<") && third.contains("switch to gemma"));
        assert!(r.contains(r#"hx-get="/ui/artifacts/A1""#));
        assert!(r.contains(r##"hx-target="#pane-content""##));
        assert!(r.contains("Note 1"));
        assert!(
            r.contains("Passage 5 about the embedder."),
            "a stand-in title"
        );
        assert!(r.contains("2 more did not bear on it"));
        assert_eq!(rail_head(&d), "Decided from 6 of 8");
        assert_eq!(rail_head(&not_held()), "Nothing decided");
        let mut one = d.clone();
        one.read = 7;
        assert!(
            rail_fragment(&one)
                .unwrap()
                .contains("1 more did not bear on it")
        );
        one.read = 6;
        assert!(!rail_fragment(&one).unwrap().contains("did not bear"));
    }

    #[test]
    fn a_split_rail_is_grouped_by_pick() {
        let mut d = held();
        d.standing = vec![("verdict".into(), Standing::Split)];
        let r = rail_fragment(&d).unwrap();
        assert_eq!(r.matches(r#"class="assoc-rule""#).count(), 2);
        let keep = r.find(r#"role="separator">keep<"#).unwrap();
        let switch = r.find(r#"role="separator">switch to gemma<"#).unwrap();
        assert!(keep < switch);
        let three = r.find("[3]").unwrap();
        assert!(three > switch, "the note against sits under its own pick");
        assert!(r.find("[4]").unwrap() < switch);
        assert!(!rail_fragment(&held()).unwrap().contains("assoc-rule"));
    }

    #[test]
    fn a_reading_row_says_what_the_passage_says_or_that_it_does_not_bear() {
        let q = choice(&["keep", "switch to gemma"]);
        let mut p = PassageRead {
            rank: 2,
            of: 8,
            artifact_id: "A2".into(),
            title: Some("Embedder comparison".into()),
            bears: 0.9,
            bearing: true,
            read: vec![("verdict".into(), chose(&q, &[0.2, 0.8]))],
        };
        let h = reading_row(&p, &q).unwrap();
        assert!(h.contains(r#"class="rail-row""#) && h.contains("[2]"));
        assert!(h.contains("Embedder comparison"));
        assert!(h.contains(r#"<span class="rail-pick mono">switch to gemma</span>"#));
        assert!(h.contains(r#"hx-get="/ui/artifacts/A2""#));
        p.bearing = false;
        p.bears = 0.1;
        let h = reading_row(&p, &q).unwrap();
        assert!(h.contains(r#"<span class="rail-pick muted">does not bear</span>"#));
        assert!(!h.contains("switch to gemma"));
    }

    #[test]
    fn a_pick_is_named_as_the_person_named_it() {
        let q = choice(&["keep", "switch to gemma"]);
        assert_eq!(pick_label(&q, &chose(&q, &[0.2, 0.8])), "switch to gemma");
        let n = Question::noul("verdict", "Keep?");
        assert_eq!(pick_label(&n, &Answer::Noul { yes: 0.3 }), "no");
        let s = scale(&["can wait", "this month", "now"]);
        assert_eq!(pick_label(&s, &scored(&[0.1, 0.2, 0.7])), "now");
        assert_eq!(standing_copy(Standing::Settled), "Settled.");
        assert_eq!(standing_copy(Standing::Leaning), "Leaning.");
        assert_eq!(standing_copy(Standing::Split), "Split.");
        assert_eq!(standing_copy(Standing::NotHeld), "");
    }

    #[test]
    fn names_are_escaped_everywhere() {
        let q = choice(&["<b>&\"'", "plain"]);
        let joint = chose(&q, &[0.7, 0.3]);
        let ev = vec![evidence(1, Some("<i>title</i>"), joint.clone())];
        let mut d = decision(q.clone(), Some(joint), chose(&q, &[0.5, 0.5]), ev);
        d.question = "<script>q</script>".into();
        let h = decision_fragment(&d, Some("wrong")).unwrap();
        let r = rail_fragment(&d).unwrap();
        let p = PassageRead {
            rank: 1,
            of: 1,
            artifact_id: "A1".into(),
            title: Some("<i>title</i>".into()),
            bears: 0.9,
            bearing: true,
            read: d.evidence[0].read.clone(),
        };
        let row = reading_row(&p, &q).unwrap();
        for s in [&h, &r, &row] {
            assert!(!s.contains("<b>&") && !s.contains("<i>") && !s.contains("<script>"));
        }
        assert!(h.contains("&#60;b&#62;&#38;&#34;&#39;: 70 of 100, 50 without notes"));
        assert!(r.contains("&#60;b&#62;&#38;&#34;&#39;"));
        assert!(h.contains("&#60;script&#62;q&#60;/script&#62;"));
    }

    #[test]
    fn the_verdict_bar_offers_three_answers_then_undo() {
        let d = held();
        let h = verdict_fragment(&verdict_bar(&d, None, None)).unwrap();
        assert!(h.contains(r#"id="decide-verdict""#) && h.contains("Was this right?"));
        for b in [
            r#"{"verdict":"right"}'"#,
            r#"{"verdict":"wrong"}'"#,
            r#"{"verdict":"nothing_here"}'"#,
        ] {
            assert!(h.contains(b), "{b}");
        }
        assert!(h.contains(">Right<") && h.contains(">Wrong<") && h.contains(">Nothing here<"));
        assert_eq!(h.matches(r#"hx-post="/ui/decide/01EV/verdict""#).count(), 3);
        assert_eq!(h.matches(r##"hx-target="#decide-verdict""##).count(), 3);
        assert_eq!(h.matches(r#"hx-swap="outerHTML""#).count(), 3);
        assert!(!h.contains("Undo"));

        let h = verdict_fragment(&verdict_bar(
            &d,
            Some("right"),
            Some(r#"{"verdict":"keep"}"#),
        ))
        .unwrap();
        assert!(h.contains("judged <b>right</b>") && h.contains(">Undo<"));
        assert!(h.contains(r#"{"verdict":"none"}"#));
        assert!(
            !h.contains(">Right<") && !h.contains("it is"),
            "a right verdict names nothing more"
        );
    }

    #[test]
    fn a_decision_not_held_is_judged_right_or_wrong_only() {
        let d = not_held();
        let h = decision_fragment(&d, None).unwrap();
        assert!(h.contains(">Right<") && h.contains(">Wrong<"));
        assert!(!h.contains(r#"{"verdict":"nothing_here"}"#));
        assert!(h.contains("Nothing here: the base does not hold this"));
        let bar = verdict_bar(&d, Some("wrong"), None);
        assert!(!bar.held && bar.remaining.is_empty());
        let h = verdict_fragment(&bar).unwrap();
        assert!(h.contains("judged <b>wrong</b>") && !h.contains("Which is it?"));
    }

    #[test]
    fn wrong_between_two_needs_no_second_press() {
        let bar = verdict_bar(
            &held(),
            Some("wrong"),
            Some(r#"{"verdict":"switch to gemma"}"#),
        );
        assert_eq!(bar.correct.as_deref(), Some("switch to gemma"));
        assert!(bar.remaining.is_empty());
        let h = verdict_fragment(&bar).unwrap();
        assert!(h.contains("judged <b>wrong</b>") && h.contains("it is switch to gemma"));
        assert!(h.contains(">Undo<") && !h.contains("Which is it?"));
    }

    #[test]
    fn wrong_among_three_asks_which_it_is_once() {
        let bar = verdict_bar(&three_levels(), Some("wrong"), None);
        assert_eq!(
            bar.remaining,
            [
                ("0".to_string(), "can wait".to_string()),
                ("2".to_string(), "now".to_string())
            ]
        );
        let h = verdict_fragment(&bar).unwrap();
        assert!(h.contains("Which is it?"));
        assert!(h.contains(">can wait<") && h.contains(">now<") && h.contains(">Not sure<"));
        assert!(h.contains(">Undo<") && !h.contains(">this month<"));
        assert!(h.contains(r#"hx-vals='{"verdict":"wrong","correct":"0"}'"#));
        assert!(h.contains(r#"hx-vals='{"verdict":"wrong","correct":"2"}'"#));
        assert!(h.contains(r#"hx-vals='{"verdict":"wrong","correct":"-"}'"#));

        let unsure = verdict_bar(&three_levels(), Some("wrong"), Some("-"));
        assert!(unsure.remaining.is_empty() && unsure.correct.is_none());
        let h = verdict_fragment(&unsure).unwrap();
        assert!(h.contains("judged <b>wrong</b>") && !h.contains("Which is it?"));
        assert!(!h.contains("it is"));
    }

    #[test]
    fn a_named_correction_is_shown_and_not_asked_again() {
        let bar = verdict_bar(&three_levels(), Some("wrong"), Some(r#"{"verdict":2}"#));
        assert_eq!(bar.correct.as_deref(), Some("now"));
        assert!(bar.remaining.is_empty());
        let h = verdict_fragment(&bar).unwrap();
        assert!(h.contains("it is now") && !h.contains("Which is it?"));
    }

    #[test]
    fn a_correction_value_cannot_break_out_of_its_attribute() {
        let q = choice(&["a'b", "c<d&", "keep"]);
        let d = decision(
            q.clone(),
            Some(chose(&q, &[0.1, 0.1, 0.8])),
            chose(&q, &[0.3, 0.3, 0.4]),
            vec![],
        );
        let h = verdict_fragment(&verdict_bar(&d, Some("wrong"), None)).unwrap();
        assert!(h.contains(r#"hx-vals='{"verdict":"wrong","correct":"a\u0027b"}'"#));
        assert!(h.contains(r#"hx-vals='{"verdict":"wrong","correct":"c\u003cd\u0026"}'"#));
        assert!(h.contains(">a&#39;b<") && h.contains(">c&#60;d&#38;<"));
    }

    use crate::core::decide::DecideRequest;
    use crate::core::decide::tests::{deciding, marked, three_base};
    use crate::web::test_support::{app_with_cookie, body_of, form, json_of};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn deciding_app(
        rule: impl Fn(&str, &[Question]) -> crate::error::Result<crate::infer::decide::Answers>
        + Send
        + Sync
        + 'static,
    ) -> (axum::Router, String, crate::core::Core) {
        let mut core = three_base().await;
        core.learn.enabled = true;
        deciding(&mut core, rule);
        let (app, cookie) = app_with_cookie(core.clone()).await;
        (app, cookie, core)
    }

    async fn post_decide(
        app: &axum::Router,
        cookie: &str,
        body: serde_json::Value,
    ) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/ui/decide")
                    .method("POST")
                    .header("cookie", cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn park(app: &axum::Router, cookie: &str, body: serde_json::Value) -> String {
        let res = post_decide(app, cookie, body).await;
        assert_eq!(res.status(), StatusCode::OK);
        json_of(res).await["id"]
            .as_str()
            .expect("an id")
            .to_string()
    }

    async fn stream(app: &axum::Router, cookie: &str, id: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ui/decide/{id}/stream"))
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// `(event, data)` in order; data as sent.
    fn frames(body: &str) -> Vec<(String, String)> {
        body.split("\n\n")
            .filter_map(|f| {
                let event = f.lines().find_map(|l| l.strip_prefix("event:"))?.trim();
                let data = f.lines().find_map(|l| l.strip_prefix("data:"))?.trim();
                Some((event.to_string(), data.to_string()))
            })
            .collect()
    }

    async fn decided(app: &axum::Router, cookie: &str, body: serde_json::Value) -> String {
        let id = park(app, cookie, body).await;
        let body = body_of(stream(app, cookie, &id).await).await;
        let (_, done) = frames(&body).pop().unwrap();
        let done: serde_json::Value =
            serde_json::from_str(&done).unwrap_or_else(|_| panic!("{body}"));
        done["event_id"].as_str().expect("recorded").to_string()
    }

    async fn verdict(app: &axum::Router, cookie: &str, id: &str, body: &str) -> String {
        let res = app
            .clone()
            .oneshot(form(&format!("/ui/decide/{id}/verdict"), cookie, body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        body_of(res).await
    }

    fn three_options() -> serde_json::Value {
        serde_json::json!({"q": "Keep bge-m3?", "questions": {"verdict": {"type": "choice",
            "criteria": {"keep": "", "switch": "", "wait": ""}}}})
    }

    #[tokio::test]
    async fn a_parked_decision_is_spent_once_and_only_by_its_owner() {
        let st = crate::web::test_support::state_over(
            crate::core::test_support::test_core().await,
            crate::config::AuthMode::Local,
        )
        .await;
        let req = DecideRequest {
            q: "Keep?".into(),
            ..Default::default()
        };
        let id = st.decide_handoff_park(req, "alice");
        assert!(st.decide_handoff_take(&id, "bob").is_none());
        assert_eq!(st.decide_handoff_take(&id, "alice").unwrap().q, "Keep?");
        assert!(st.decide_handoff_take(&id, "alice").is_none(), "spent");
    }

    #[tokio::test]
    async fn posting_a_question_parks_it_and_returns_an_id() {
        let (app, cookie, _) = deciding_app(marked(0.9)).await;
        let id = park(&app, &cookie, serde_json::json!({"q": "Keep bge-m3?"})).await;
        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn an_empty_question_is_refused() {
        let (app, cookie, _) = deciding_app(marked(0.9)).await;
        let res = post_decide(&app, &cookie, serde_json::json!({"q": "  "})).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn two_questions_are_refused_in_the_web_ui() {
        let (app, cookie, _) = deciding_app(marked(0.9)).await;
        let body = serde_json::json!({"q": "Keep?", "questions": {
            "a": {"type": "noul"}, "b": {"type": "noul"}}});
        let res = post_decide(&app, &cookie, body).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn the_stream_sends_rows_as_they_are_read_and_then_the_result() {
        let (app, cookie, _) = deciding_app(marked(0.9)).await;
        let id = park(&app, &cookie, serde_json::json!({"q": "Keep bge-m3?"})).await;
        let res = stream(&app, &cookie, &id).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_of(res).await;
        let f = frames(&body);
        let names: Vec<&str> = f.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["retrieved", "read", "read", "read", "done"],
            "{body}"
        );
        let v = |i: usize| serde_json::from_str::<serde_json::Value>(&f[i].1).unwrap();
        assert_eq!(v(0), serde_json::json!({"passages": 3}));
        for i in 1..4 {
            let r = v(i);
            assert_eq!(r["of"], 3);
            assert!(r["rank"].is_number() && r["bearing"].is_boolean(), "{r}");
            assert!(r["row"].as_str().unwrap().contains("rail-row"), "{r}");
        }
        let done = v(4);
        assert!(done["html"].as_str().unwrap().contains("bar-track"));
        assert!(done["rail"].as_str().unwrap().contains("rail-row"));
        assert_eq!(done["rail_head"], "Decided from 2 of 3");
        assert!(done["event_id"].is_string(), "{done}");
    }

    #[tokio::test]
    async fn a_spent_id_is_not_found() {
        let (app, cookie, _) = deciding_app(marked(0.9)).await;
        let id = park(&app, &cookie, serde_json::json!({"q": "Keep bge-m3?"})).await;
        body_of(stream(&app, &cookie, &id).await).await;
        assert_eq!(
            stream(&app, &cookie, &id).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            stream(&app, &cookie, "nope").await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn an_error_ends_the_stream_in_one_sentence() {
        let (app, cookie, _) =
            deciding_app(|_, _| Err(crate::error::Error::Internal("the rule erred".into()))).await;
        let id = park(&app, &cookie, serde_json::json!({"q": "Keep bge-m3?"})).await;
        let body = body_of(stream(&app, &cookie, &id).await).await;
        let (name, data) = frames(&body).pop().unwrap();
        assert_eq!(name, "error", "{body}");
        assert!(
            !data.contains("the rule erred"),
            "an internal error stays in the log"
        );
    }

    #[tokio::test]
    async fn a_verdict_is_judged_and_undone_from_the_bar() {
        let (app, cookie, _) = deciding_app(marked(0.9)).await;
        let id = decided(&app, &cookie, serde_json::json!({"q": "Keep bge-m3?"})).await;
        let h = verdict(&app, &cookie, &id, "verdict=right").await;
        assert!(
            h.contains("judged <b>right</b>") && h.contains(">Undo<"),
            "{h}"
        );
        let h = verdict(&app, &cookie, &id, "verdict=none").await;
        assert!(
            h.contains(">Right<") && h.contains(">Wrong<") && h.contains(">Nothing here<"),
            "{h}"
        );
    }

    #[tokio::test]
    async fn wrong_on_three_options_asks_then_takes_the_answer() {
        let (app, cookie, core) = deciding_app(marked(0.9)).await;
        let id = decided(&app, &cookie, three_options()).await;
        let h = verdict(&app, &cookie, &id, "verdict=wrong").await;
        assert!(
            h.contains("Which is it?") && h.contains(">switch<") && h.contains(">wait<"),
            "{h}"
        );
        let h = verdict(&app, &cookie, &id, "verdict=wrong&correct=switch").await;
        assert!(
            h.contains("it is switch") && !h.contains("Which is it?"),
            "{h}"
        );
        let d = core.store.decision(&id).await.unwrap().unwrap();
        assert_eq!(d.correct.as_deref(), Some(r#"{"verdict":"switch"}"#));
        let h = verdict(&app, &cookie, &id, "verdict=wrong&correct=-").await;
        assert!(
            h.contains("judged <b>wrong</b>") && !h.contains("it is"),
            "{h}"
        );
        assert!(!h.contains("Which is it?"), "{h}");
    }

    #[tokio::test]
    async fn a_level_is_corrected_by_its_index() {
        let (app, cookie, core) = deciding_app(marked(0.9)).await;
        let body = serde_json::json!({"q": "How soon?", "questions": {"verdict": {"type": "score",
            "criteria": ["can wait", "this month", "now"]}}});
        let id = decided(&app, &cookie, body).await;
        let h = verdict(&app, &cookie, &id, "verdict=wrong&correct=2").await;
        assert!(h.contains("it is now"), "{h}");
        let d = core.store.decision(&id).await.unwrap().unwrap();
        assert_eq!(d.correct.as_deref(), Some(r#"{"verdict":2}"#));
    }

    #[tokio::test]
    async fn a_right_verdict_from_the_bar_confirms_the_agreeing_notes() {
        let (app, cookie, core) = deciding_app(marked(0.9)).await;
        let id = decided(&app, &cookie, serde_json::json!({"q": "Keep bge-m3?"})).await;
        let d = core.store.decision(&id).await.unwrap().unwrap();
        let bearing: Vec<String> = d
            .evidence
            .iter()
            .filter(|e| e.bearing)
            .map(|e| e.artifact_id.clone())
            .collect();
        assert!(!bearing.is_empty());
        verdict(&app, &cookie, &id, "verdict=right").await;
        for a in &bearing {
            let got = core.store.get_artifact(a).await.unwrap();
            assert!(got.last_verified_at.is_some(), "{a} was not confirmed");
        }
    }
}
