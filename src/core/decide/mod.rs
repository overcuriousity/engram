//! Decide: each passage read alone, then the bearing ones together; the verdict and how settled it stands.

pub mod standing;
pub mod state;
pub mod stream;
pub mod verdict;
pub mod wire;

use super::Core;
use super::search::SearchQuery;
use crate::error::{Error, Result};
use crate::infer::budget::TokenCounter;
use crate::infer::decide::{
    Answer, Answers, Kind, Question, budget, decide_swapped, swapped_questions, trim, validate,
};
use crate::store::decisions::{NewDecision, NewEvidence};
use crate::store::feedback::{Door, Origin};
pub use standing::Standing;
use state::{BEARS_KEY, Excerpt, bears_question, joint_state, prior_state, single_state};
pub use stream::{DecideEvent, PassageRead};
use tokio_stream::StreamExt;

pub const DEFAULT_LIMIT: usize = 8;
pub const MAX_LIMIT: usize = 16;
pub const MAX_CALLER_QUESTIONS: usize = 8;
pub const MAX_SITUATION_TOKENS: usize = 2000;
/// What tokenising the joined state may cost beyond its counted parts.
const SEAM_TOKENS: usize = 32;

#[derive(Debug, Clone, Default)]
pub struct DecideRequest {
    pub q: String,
    /// Empty: one noul keyed "verdict"; empty instructions become `q`.
    pub questions: Vec<Question>,
    pub situation: Option<String>,
    pub limit: Option<usize>,
    pub tags: Vec<String>,
    pub category: Option<String>,
    pub explain: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub answer: Answer,
    pub swapped_agree: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    /// Position among the evidence, from 1.
    pub n: usize,
    pub artifact_id: String,
    pub title: Option<String>,
    pub text: String,
    pub bears: f64,
    /// It went into the joint read.
    pub joint: bool,
    /// In question order.
    pub read: Vec<(String, Answer)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub question: String,
    /// As asked, defaults filled.
    pub questions: Vec<Question>,
    /// `None`: not held.
    pub answers: Option<Vec<(String, Verdict)>>,
    pub prior: Vec<(String, Answer)>,
    pub standing: Vec<(String, Standing)>,
    pub evidence: Vec<Evidence>,
    pub read: usize,
    pub dropped: usize,
    /// As the server named itself on the last call.
    pub model: String,
    pub event_id: Option<String>,
    /// With `explain`.
    pub state: Option<String>,
    /// Every passage read, by rank; stored, not in the JSON.
    pub reads: Vec<PassageRead>,
}

impl Decision {
    pub fn not_held(&self) -> bool {
        self.answers.is_none()
    }
}

struct Asked {
    q: String,
    questions: Vec<Question>,
    situation: Option<String>,
    limit: usize,
}

fn refuse(sentence: &str) -> Error {
    Error::Validation(format!("decide: {sentence}"))
}

fn rejected(detail: String) -> Error {
    Error::InferenceRejected {
        role: "decide",
        detail,
    }
}

/// The questions as the model is asked them, so a stream can name a choice's options as `done` does.
pub(crate) fn normalised_questions(req: &DecideRequest) -> Result<Vec<Question>> {
    let q = trim(&req.q).to_string();
    if q.is_empty() {
        return Err(refuse("the question is empty."));
    }
    if req.questions.len() > MAX_CALLER_QUESTIONS {
        return Err(refuse(&format!(
            "ask at most {MAX_CALLER_QUESTIONS} questions."
        )));
    }
    let mut questions: Vec<Question> = req
        .questions
        .iter()
        .map(|c| {
            let instructions = match trim(&c.instructions) {
                "" => q.clone(),
                i => i.to_string(),
            };
            let kind = match &c.kind {
                Kind::Choice { options } => Kind::Choice {
                    options: options
                        .iter()
                        .map(|(n, d)| (trim(n).to_string(), trim(d).to_string()))
                        .collect(),
                },
                Kind::Score { levels } => Kind::Score {
                    levels: levels.iter().map(|l| trim(l).to_string()).collect(),
                },
                Kind::Noul => Kind::Noul,
            };
            Question {
                key: c.key.clone(),
                kind,
                instructions,
            }
        })
        .collect();
    if questions.is_empty() {
        questions.push(Question::noul("verdict", &q));
    }
    if questions.iter().any(|c| c.key == BEARS_KEY) {
        return Err(refuse(&format!(
            "the key \"{BEARS_KEY}\" is the decision's own; use another."
        )));
    }
    validate(&questions)?;
    Ok(questions)
}

/// Trimmed and defaulted, so the decision records the questions exactly as the model was asked them.
fn normalise(req: &DecideRequest, counter: &TokenCounter) -> Result<Asked> {
    let questions = normalised_questions(req)?;
    let q = trim(&req.q).to_string();
    let situation = req
        .situation
        .as_deref()
        .map(trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(s) = &situation {
        let n = counter.count(s);
        if n > MAX_SITUATION_TOKENS {
            return Err(refuse(&format!(
                "the situation is {n} tokens and {MAX_SITUATION_TOKENS} fit."
            )));
        }
    }
    Ok(Asked {
        q,
        questions,
        situation,
        limit: req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
    })
}

/// The caller's answers in question order; a missing one is the model's fault.
fn in_order(questions: &[Question], got: &Answers) -> Result<Vec<(String, Answer)>> {
    questions
        .iter()
        .map(|q| {
            got.answers
                .get(&q.key)
                .map(|a| (q.key.clone(), a.clone()))
                .ok_or_else(|| rejected(format!("the answer has nothing for \"{}\"", q.key)))
        })
        .collect()
}

struct Passage {
    artifact_id: String,
    title: Option<String>,
    text: String,
    day: String,
}

impl Passage {
    fn excerpt<'a>(&'a self, text: &'a str) -> Excerpt<'a> {
        Excerpt {
            title: self.title.as_deref(),
            day: &self.day,
            text,
        }
    }
}

impl Core {
    /// A collector over `decide_events`, so there is exactly one account of what deciding means.
    pub async fn decide(&self, req: &DecideRequest, origin: impl Into<Origin>) -> Result<Decision> {
        let s = self.decide_events(req, origin);
        tokio::pin!(s);
        let mut done = None;
        while let Some(ev) = s.next().await {
            if let DecideEvent::Done(d) = ev? {
                done = Some(*d);
            }
        }
        done.ok_or_else(|| Error::Internal("decide produced no decision".into()))
    }

    /// One decision as it happens. `'static`: an SSE response outlives its handler, so it owns its inputs.
    pub fn decide_events<O: Into<Origin>>(
        &self,
        req: &DecideRequest,
        origin: O,
    ) -> impl tokio_stream::Stream<Item = Result<DecideEvent>> + 'static + use<O> {
        let core = self.clone();
        let req = req.clone();
        let origin = origin.into();

        async_stream::try_stream! {
            let asked = normalise(&req, &core.counter)?;
            let (questions, situation) = (&asked.questions, asked.situation.as_deref());
            let cfg = core.decide_cfg.clone();

            // Every spawned read holds a clone: a read still in flight keeps the lane until it is aborted.
            let lane = std::sync::Arc::new(core.gate.interactive());

            let passages = core.decide_passages(&req, &asked).await?;
            let of = passages.len();
            yield DecideEvent::Retrieved { passages: of };

            let decider = core.decider.clone();
            let got = decider.decide(&prior_state(situation), questions).await?;
            let prior = in_order(questions, &got)?;
            let mut model = got.model;

            let mut with_bears = questions.clone();
            with_bears.push(bears_question(&asked.q));
            let window = decider.context_tokens();
            let room = budget(&core.counter, window, &with_bears).saturating_sub(SEAM_TOKENS);
            // Dropping the set aborts every read still running, when the reader leaves.
            let mut set = tokio::task::JoinSet::new();
            for (i, p) in passages.iter().enumerate() {
                // The frame is counted, not allowed for: a long title would not fit a fixed allowance.
                let frame = core.counter.count(&single_state(situation, &p.excerpt("")));
                let text = core
                    .counter
                    .cut(&p.text, room.saturating_sub(frame), true)
                    .unwrap_or_default();
                let state = single_state(situation, &p.excerpt(&text));
                let (decider, asked, lane) = (decider.clone(), with_bears.clone(), lane.clone());
                set.spawn(async move {
                    let _lane = lane;
                    (i, decider.decide(&state, &asked).await)
                });
            }
            let mut reads: Vec<PassageRead> = Vec::with_capacity(of);
            while let Some(joined) = set.join_next().await {
                let (i, got) = joined.map_err(|e| Error::Internal(format!("decide: a read did not finish: {e}")))?;
                // One failed read fails the decision: a partial verdict would be a guess.
                let got = got?;
                let bears = match got.answers.get(BEARS_KEY) {
                    Some(Answer::Noul { yes }) => *yes,
                    _ => Err(rejected(format!("the answer has no noul for \"{BEARS_KEY}\"")))?,
                };
                let p = &passages[i];
                let r = PassageRead {
                    rank: i + 1,
                    of,
                    artifact_id: p.artifact_id.clone(),
                    title: p.title.clone(),
                    bears,
                    bearing: bears >= cfg.bears_min,
                    read: in_order(questions, &got)?,
                };
                model = got.model;
                reads.push(r.clone());
                yield DecideEvent::Read(Box::new(r));
            }
            reads.sort_by_key(|r| r.rank);

            let mut bearing: Vec<&PassageRead> = reads.iter().filter(|r| r.bearing).collect();
            bearing.sort_by(|a, b| b.bears.total_cmp(&a.bears).then(a.rank.cmp(&b.rank)));

            let mut decision = Decision {
                question: asked.q.clone(),
                questions: questions.clone(),
                answers: None,
                prior,
                standing: questions.iter().map(|q| (q.key.clone(), Standing::NotHeld)).collect(),
                evidence: Vec::new(),
                read: of,
                dropped: 0,
                model,
                event_id: None,
                state: None,
                reads: Vec::new(),
            };
            if bearing.is_empty() {
                decision.reads = reads;
                core.record_decision(&req, &asked, &origin, &mut decision).await;
                yield DecideEvent::Done(Box::new(decision));
                return;
            }

            let excerpts: Vec<Excerpt<'_>> = bearing
                .iter()
                .map(|r| {
                    let p = &passages[r.rank - 1];
                    p.excerpt(&p.text)
                })
                .collect();
            let tokens = cfg
                .joint_tokens
                .min(budget(&core.counter, window, &swapped_questions(questions)));
            let (state, packed) = joint_state(situation, &excerpts, &core.counter, tokens);
            let joint = decide_swapped(decider.as_ref(), &state, questions).await?;

            let evidence: Vec<Evidence> = bearing
                .iter()
                .enumerate()
                .map(|(j, r)| Evidence {
                    n: j + 1,
                    artifact_id: r.artifact_id.clone(),
                    title: r.title.clone(),
                    text: passages[r.rank - 1].text.clone(),
                    bears: r.bears,
                    joint: j < packed,
                    read: r.read.clone(),
                })
                .collect();
            let answers: Vec<(String, Verdict)> = in_order(questions, &joint.answers)?
                .into_iter()
                .map(|(k, answer)| {
                    let swapped_agree = joint.agree.get(&k).copied();
                    (k, Verdict { answer, swapped_agree })
                })
                .collect();
            decision.standing = questions
                .iter()
                .zip(&answers)
                .enumerate()
                .map(|(i, (q, (k, v)))| {
                    let each: Vec<(f64, &Answer)> =
                        evidence.iter().map(|e| (e.bears, &e.read[i].1)).collect();
                    let s = standing::standing(q, &v.answer, v.swapped_agree, &each, &cfg);
                    (k.clone(), s)
                })
                .collect();
            decision.answers = Some(answers);
            decision.dropped = evidence.len() - packed;
            decision.evidence = evidence;
            decision.model = joint.answers.model;
            decision.state = req.explain.then_some(state);
            decision.reads = reads;
            core.record_decision(&req, &asked, &origin, &mut decision).await;
            yield DecideEvent::Done(Box::new(decision));
        }
    }

    /// Where an ask would be recorded; a failure is logged and the decision still goes out.
    async fn record_decision(
        &self,
        req: &DecideRequest,
        asked: &Asked,
        origin: &Origin,
        d: &mut Decision,
    ) {
        if !(self.learn.enabled && matches!(origin.door, Door::Ui | Door::Cli | Door::App)) {
            return;
        }
        let held = !d.not_held();
        let evidence = d
            .reads
            .iter()
            .map(|r| {
                let placed = d.evidence.iter().find(|e| e.artifact_id == r.artifact_id);
                NewEvidence {
                    rank: r.rank,
                    artifact_id: r.artifact_id.clone(),
                    bears: r.bears,
                    bearing: r.bearing,
                    joint: placed.is_some_and(|e| e.joint),
                    n: placed.map(|e| e.n),
                    read: wire::read_text(&d.questions, &r.read),
                }
            })
            .collect::<Vec<_>>();
        let cited: Vec<String> = if held {
            evidence
                .iter()
                .filter(|e| e.joint)
                .map(|e| e.artifact_id.clone())
                .collect()
        } else {
            Vec::new()
        };
        let new = NewDecision {
            question: d.question.clone(),
            questions: wire::questions_json(&d.questions),
            situation: asked.situation.clone(),
            scope: origin.scope.clone(),
            filters: serde_json::json!({
                "tags": req.tags,
                "category": req.category,
                "limit": req.limit.unwrap_or(DEFAULT_LIMIT),
            })
            .to_string(),
            answers: wire::answers_text(d),
            prior: wire::prior_text(d),
            standing: wire::standing_text(d),
            not_held: !held,
            model: d.model.clone(),
            read: d.read,
            dropped: d.dropped,
            query_vec: self.cached_query_vector(&asked.q).unwrap_or_default(),
            embed_model: self.embedder.model().to_string(),
            evidence,
        };
        match self.store.record_decision(new).await {
            Ok(id) => d.event_id = Some(id),
            Err(e) => tracing::warn!(error = %e, "could not record the decision"),
        }
        // Off the recording's success: an insert that fails does not unmake what was weighed.
        self.mark_artifacts_cited(cited);
    }

    /// Asks search for twice the limit, so dropping retired notes still leaves `limit` to read.
    async fn decide_passages(&self, req: &DecideRequest, asked: &Asked) -> Result<Vec<Passage>> {
        let (hits, _) = self
            .search_with(
                &SearchQuery {
                    q: asked.q.clone(),
                    limit: (asked.limit * 2).min(50),
                    tags: req.tags.clone(),
                    category: req.category.clone(),
                    // As deliberate as asking.
                    mark: true,
                    include_deprecated: false,
                    include_superseded: false,
                    rerank: true,
                    explain: false,
                },
                None,
                Door::Ask,
            )
            .await?;
        let hits: Vec<_> = hits
            .into_iter()
            .filter(|h| !h.retired)
            .take(asked.limit)
            .collect();
        let ids: Vec<String> = hits.iter().map(|h| h.artifact_id.clone()).collect();
        let born = self.store.created_at_among(&ids).await?;
        Ok(hits
            .into_iter()
            .map(|h| Passage {
                day: born
                    .get(&h.artifact_id)
                    .map(|t| crate::fmt::fmt_day(*t))
                    .unwrap_or_default(),
                artifact_id: h.artifact_id,
                title: h.title,
                text: h.text,
            })
            .collect())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::Core;
    use crate::core::ingest::Capture;
    use crate::core::test_support::test_core;
    use crate::error::Error;
    use crate::infer::decide::{Answer, Answers, Decider, Kind, Question};
    use crate::infer::fake::RuleDecider;
    use crate::store::feedback::Door;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio_stream::StreamExt;

    const MARK: &str = "zebracrossing";

    async fn base(notes: &[(&str, &str)]) -> Core {
        let core = test_core().await;
        for (title, text) in notes {
            let mut c = Capture::new(*text, "ui");
            c.title_hint = Some(title.to_string());
            core.ingest_capture(c).await.unwrap();
        }
        crate::jobs::test_support::drain(&core).await;
        core
    }

    pub(crate) fn deciding(
        core: &mut Core,
        rule: impl Fn(&str, &[Question]) -> crate::error::Result<Answers> + Send + Sync + 'static,
    ) -> Arc<RuleDecider> {
        let d = Arc::new(RuleDecider::new(rule));
        core.decider = d.clone();
        d
    }

    fn answers(qs: &[Question], f: impl Fn(&Question) -> Answer) -> Answers {
        Answers {
            answers: qs.iter().map(|q| (q.key.clone(), f(q))).collect(),
            model: "rule".into(),
            input_tokens: 0,
        }
    }

    /// `p` on the option named `name`, the rest shared.
    fn favour(options: &[(String, String)], name: &str, p: f64) -> Answer {
        let rest = (1.0 - p) / (options.len() - 1) as f64;
        Answer::Choice {
            pick: name.into(),
            probabilities: options
                .iter()
                .map(|(n, _)| if n == name { p } else { rest })
                .collect(),
            confidence: p,
        }
    }

    /// `bears` by the marker; a noul says `yes`, a choice favours its first option, a score its first level.
    pub(crate) fn marked(yes: f64) -> impl Fn(&str, &[Question]) -> crate::error::Result<Answers> {
        move |state, qs| {
            Ok(answers(qs, |q| match &q.kind {
                _ if q.key == "bears" => Answer::Noul {
                    yes: if state.contains(MARK) { 0.95 } else { 0.02 },
                },
                Kind::Noul => Answer::Noul { yes },
                Kind::Choice { options } => favour(options, &options[0].0, yes),
                Kind::Score { levels } => Answer::Score {
                    score: 0.0,
                    probabilities: (0..levels.len())
                        .map(|i| if i == 0 { yes } else { 0.0 })
                        .collect(),
                    confidence: yes,
                },
            }))
        }
    }

    fn req(q: &str) -> DecideRequest {
        DecideRequest {
            q: q.into(),
            ..Default::default()
        }
    }

    fn three() -> [(&'static str, String); 3] {
        [
            (
                "Reindex",
                format!("The embedder reindex took {MARK} an hour."),
            ),
            (
                "Sizing",
                format!("Qdrant sizing is fine {MARK} with bge-m3."),
            ),
            ("Garden", "The tomatoes need water on Sunday.".to_string()),
        ]
    }

    pub(crate) async fn three_base() -> Core {
        let notes = three();
        let refs: Vec<(&str, &str)> = notes.iter().map(|(t, x)| (*t, x.as_str())).collect();
        base(&refs).await
    }

    fn choice(options: &[&str], instructions: &str) -> Question {
        Question {
            key: "verdict".into(),
            kind: Kind::Choice {
                options: options
                    .iter()
                    .map(|n| (n.to_string(), String::new()))
                    .collect(),
            },
            instructions: instructions.into(),
        }
    }

    #[tokio::test]
    async fn a_held_question_returns_a_verdict_and_what_it_rests_on() {
        let mut core = three_base().await;
        deciding(&mut core, marked(0.9));
        let d = core.decide(&req("Keep bge-m3?"), Door::Api).await.unwrap();
        assert_eq!(
            d.answers,
            Some(vec![(
                "verdict".to_string(),
                Verdict {
                    answer: Answer::Noul { yes: 0.9 },
                    swapped_agree: None
                }
            )])
        );
        assert_eq!(d.standing, vec![("verdict".to_string(), Standing::Settled)]);
        assert_eq!(d.evidence.len(), 2);
        assert_eq!(
            d.evidence.iter().map(|e| e.n).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(d.evidence.iter().all(|e| e.joint));
        assert_eq!((d.read, d.dropped), (3, 0));
        for e in &d.evidence {
            let keys: Vec<&str> = e.read.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(keys, vec!["verdict"]);
        }
        assert_eq!(d.prior.len(), 1);
        assert_eq!(d.prior[0].0, "verdict");
        assert_eq!(d.questions, vec![Question::noul("verdict", "Keep bge-m3?")]);
        assert_eq!(d.model, "rule");
        assert!(!d.not_held() && d.event_id.is_none());
        assert_eq!(d.reads.len(), 3);
    }

    #[tokio::test]
    async fn nothing_bearing_means_no_verdict_whatever_the_model_would_say() {
        let mut core = three_base().await;
        let dec = deciding(&mut core, |_, qs| {
            Ok(answers(qs, |q| Answer::Noul {
                yes: if q.key == "bears" { 0.02 } else { 0.99 },
            }))
        });
        let d = core.decide(&req("Keep bge-m3?"), Door::Api).await.unwrap();
        assert!(d.not_held());
        assert_eq!(d.answers, None);
        assert_eq!(d.standing, vec![("verdict".to_string(), Standing::NotHeld)]);
        assert!(d.evidence.is_empty());
        assert_eq!(d.read, 3);
        assert_eq!(d.prior.len(), 1);
        assert!(dec.requests().iter().all(|(s, _)| !s.contains("Excerpt 1")));
    }

    #[tokio::test]
    async fn an_empty_base_is_not_held_after_the_prior_alone() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, marked(0.9));
        let d = core.decide(&req("Keep bge-m3?"), Door::Api).await.unwrap();
        assert!(d.not_held());
        assert_eq!((d.read, dec.calls()), (0, 1));
    }

    #[tokio::test]
    async fn each_passage_is_read_alone_with_the_bears_question() {
        let mut core = three_base().await;
        let dec = deciding(&mut core, marked(0.9));
        core.decide(&req("Keep bge-m3?"), Door::Api).await.unwrap();
        let singles: Vec<_> = dec
            .requests()
            .into_iter()
            .filter(|(s, _)| s.contains("Excerpt:"))
            .collect();
        assert_eq!(singles.len(), 3);
        for (s, qs) in singles {
            let keys: Vec<&str> = qs.iter().map(|q| q.key.as_str()).collect();
            assert_eq!(keys, vec!["verdict", "bears"]);
            assert_eq!(s.matches("Excerpt").count(), 1, "{s}");
        }
    }

    #[tokio::test]
    async fn the_situation_leads_every_state() {
        let mut core = three_base().await;
        let dec = deciding(&mut core, marked(0.9));
        let r = DecideRequest {
            situation: Some("12 GB.".into()),
            ..req("Keep bge-m3?")
        };
        core.decide(&r, Door::Api).await.unwrap();
        assert_eq!(dec.calls(), 5);
        assert!(
            dec.requests()
                .iter()
                .all(|(s, _)| s.starts_with("Situation:\n12 GB."))
        );
    }

    #[tokio::test]
    async fn evidence_is_ordered_by_bearing() {
        let mut core = base(&[
            ("A", "The alphamarker note about embedders."),
            ("B", "The bravomarker note about embedders."),
            ("C", "The charliemarker note about embedders."),
        ])
        .await;
        deciding(&mut core, |state, qs| {
            let b = [
                ("alphamarker", 0.6),
                ("bravomarker", 0.9),
                ("charliemarker", 0.7),
            ]
            .iter()
            .find(|(m, _)| state.contains(m))
            .map_or(0.02, |(_, b)| *b);
            Ok(answers(qs, |q| Answer::Noul {
                yes: if q.key == "bears" { b } else { 0.9 },
            }))
        });
        let d = core.decide(&req("Embedders?"), Door::Api).await.unwrap();
        let bears: Vec<f64> = d.evidence.iter().map(|e| e.bears).collect();
        assert_eq!(bears, vec![0.9, 0.7, 0.6]);
    }

    #[tokio::test]
    async fn a_choice_is_weighed_with_its_options_swapped() {
        let mut core = three_base().await;
        let dec = deciding(&mut core, |state, qs| {
            Ok(answers(qs, |q| match &q.kind {
                _ if q.key == "bears" => Answer::Noul {
                    yes: if state.contains(MARK) { 0.95 } else { 0.02 },
                },
                Kind::Choice { options } => favour(options, "keep", 0.9),
                _ => Answer::Noul { yes: 0.5 },
            }))
        });
        let r = DecideRequest {
            questions: vec![choice(&["keep", "switch"], "Keep it?")],
            ..req("Keep bge-m3?")
        };
        let d = core.decide(&r, Door::Api).await.unwrap();
        let joint = dec
            .requests()
            .into_iter()
            .find(|(s, _)| s.contains("Excerpt 1"))
            .unwrap();
        let keys: Vec<&str> = joint.1.iter().map(|q| q.key.as_str()).collect();
        assert_eq!(keys, vec!["q0", "q0r"]);
        assert_eq!(d.answers.unwrap()[0].1.swapped_agree, Some(true));
    }

    #[tokio::test]
    async fn option_names_with_surrounding_whitespace_are_trimmed_and_still_picked() {
        let mut core = three_base().await;
        let dec = deciding(&mut core, |state, qs| {
            Ok(answers(qs, |q| match &q.kind {
                _ if q.key == "bears" => Answer::Noul {
                    yes: if state.contains(MARK) { 0.95 } else { 0.02 },
                },
                Kind::Choice { options } => favour(options, "keep", 0.9),
                _ => Answer::Noul { yes: 0.5 },
            }))
        });
        let mut q = choice(&[" keep ", "switch\n"], "  Keep it?  ");
        if let Kind::Choice { options } = &mut q.kind {
            options[0].1 = " the same ".into();
        }
        let r = DecideRequest {
            questions: vec![q],
            ..req("Keep bge-m3?")
        };
        let d = core.decide(&r, Door::Api).await.unwrap();
        let want = Question {
            key: "verdict".into(),
            kind: Kind::Choice {
                options: vec![
                    ("keep".into(), "the same".into()),
                    ("switch".into(), String::new()),
                ],
            },
            instructions: "Keep it?".into(),
        };
        assert_eq!(d.questions, vec![want.clone()]);
        assert!(dec.requests()[0].1.contains(&want));
        assert_eq!(d.standing, vec![("verdict".to_string(), Standing::Settled)]);
    }

    #[tokio::test]
    async fn evidence_past_the_joint_budget_is_dropped_and_counted() {
        let words = |w: &str| format!("{MARK} {w} {}", "note ".repeat(75));
        let notes = [
            ("A", words("alpha")),
            ("B", words("bravo")),
            ("C", words("charlie")),
            ("D", words("delta")),
        ];
        let refs: Vec<(&str, &str)> = notes.iter().map(|(t, x)| (*t, x.as_str())).collect();
        let mut core = base(&refs).await;
        core.decide_cfg.joint_tokens = 300;
        deciding(&mut core, marked(0.9));
        let d = core.decide(&req("Which word?"), Door::Api).await.unwrap();
        assert_eq!(d.evidence.len(), 4);
        assert_eq!(d.dropped, 2);
        assert_eq!(d.evidence.iter().filter(|e| !e.joint).count(), 2);
        assert!(d.evidence[..2].iter().all(|e| e.joint));
    }

    #[tokio::test]
    async fn a_passage_longer_than_the_window_is_cut_not_refused() {
        let long = format!("{MARK} {}", "word ".repeat(3000));
        // A heading of its own size too: the title is not inside the allowance.
        let title = "A title of many words ".repeat(8);
        let mut core = base(&[(title.as_str(), long.as_str())]).await;
        let dec = deciding(&mut core, marked(0.9));
        // Chunking keeps a passage near 400 tokens; this window leaves a single read under 200.
        dec.set_context_tokens(512);
        let r = DecideRequest {
            situation: Some("The GPU has 12 GB.".into()),
            ..req("Words?")
        };
        let d = core.decide(&r, Door::Api).await.unwrap();
        assert!(!d.not_held());
        let single = dec
            .requests()
            .into_iter()
            .find(|(s, _)| s.contains("Excerpt:"))
            .unwrap();
        assert!(
            !single.0.contains(&d.evidence[0].text),
            "the passage went in whole"
        );
        assert!(single.0.contains(MARK));
        let c = crate::infer::budget::TokenCounter::default();
        for (s, qs) in dec.requests() {
            assert!(
                c.count(&s) <= crate::infer::decide::budget(&c, 512, &qs),
                "{}",
                c.count(&s)
            );
        }
    }

    #[tokio::test]
    async fn bad_requests_are_refused_before_any_call() {
        let mut core = three_base().await;
        let dec = deciding(&mut core, marked(0.9));
        let nine = (0..9)
            .map(|i| Question::noul(&format!("q{i}"), "Is it?"))
            .collect();
        let mut long = String::new();
        let c = crate::infer::budget::TokenCounter::default();
        while c.count(&long) < 2001 {
            long.push_str("word ");
        }
        let bad = [
            req(""),
            req("  \n"),
            DecideRequest {
                questions: nine,
                ..req("x?")
            },
            DecideRequest {
                questions: vec![Question::noul("bears", "Is it?")],
                ..req("x?")
            },
            DecideRequest {
                questions: vec![choice(&["only"], "One?")],
                ..req("x?")
            },
            DecideRequest {
                situation: Some(long),
                ..req("x?")
            },
        ];
        for r in bad {
            let e = core.decide(&r, Door::Api).await;
            assert!(matches!(e, Err(Error::Validation(_))), "{e:?}");
            let s = core.decide_events(&r, Door::Api);
            tokio::pin!(s);
            assert!(matches!(s.next().await, Some(Err(Error::Validation(_)))));
        }
        assert_eq!(dec.calls(), 0);
    }

    #[tokio::test]
    async fn a_question_without_instructions_asks_q() {
        let mut core = three_base().await;
        let dec = deciding(&mut core, marked(0.9));
        let r = DecideRequest {
            questions: vec![choice(&["keep", "switch"], "")],
            ..req("Keep bge-m3?")
        };
        let d = core.decide(&r, Door::Api).await.unwrap();
        assert_eq!(d.questions[0].instructions, "Keep bge-m3?");
        assert!(
            dec.requests()
                .iter()
                .all(|(_, qs)| qs[0].instructions == "Keep bge-m3?")
        );
    }

    #[tokio::test]
    async fn the_limit_defaults_to_eight_and_stops_at_sixteen() {
        let notes: Vec<(String, String)> = (0..20)
            .map(|i| {
                (
                    format!("N{i}"),
                    format!("Note number {i} says {MARK} item{i}."),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = notes
            .iter()
            .map(|(t, x)| (t.as_str(), x.as_str()))
            .collect();
        let mut core = base(&refs).await;
        deciding(&mut core, marked(0.9));
        let d = core.decide(&req("Which note?"), Door::Api).await.unwrap();
        assert_eq!(d.read, 8);
        let r = DecideRequest {
            limit: Some(40),
            ..req("Which note?")
        };
        assert_eq!(core.decide(&r, Door::Api).await.unwrap().read, 16);
    }

    #[tokio::test]
    async fn retired_notes_are_not_read() {
        let mut core = test_core().await;
        for t in ["the invoice for august", "invoice terms and conditions"] {
            core.ingest_capture(Capture::new(t, "ui")).await.unwrap();
        }
        let out = core
            .ingest_capture(Capture::new(
                "remind me friday to send the invoice",
                "reminder",
            ))
            .await
            .unwrap();
        crate::jobs::test_support::drain(&core).await;
        core.store
            .retire_corpus(&out.id, crate::core::search::now_secs())
            .await
            .unwrap();
        let dec = deciding(&mut core, marked(0.9));
        let d = core.decide(&req("invoice"), Door::Api).await.unwrap();
        assert_eq!(d.read, 2);
        assert!(dec.requests().iter().all(|(s, _)| !s.contains("friday")));
    }

    #[tokio::test]
    async fn a_failed_read_fails_the_decision() {
        let mut core = three_base().await;
        let ok = marked(0.9);
        deciding(&mut core, move |state, qs| {
            if state.contains("tomatoes") {
                return Err(Error::Inference {
                    role: "decide",
                    detail: "down".into(),
                });
            }
            ok(state, qs)
        });
        assert!(core.decide(&req("Keep?"), Door::Api).await.is_err());
        let s = core.decide_events(&req("Keep?"), Door::Api);
        tokio::pin!(s);
        let mut saw_err = false;
        while let Some(ev) = s.next().await {
            match ev {
                Ok(DecideEvent::Done(_)) => panic!("a decision from a failed read"),
                Err(_) => saw_err = true,
                Ok(_) => {}
            }
        }
        assert!(saw_err);
    }

    #[tokio::test]
    async fn explain_carries_the_joint_state() {
        let mut core = three_base().await;
        deciding(&mut core, marked(0.9));
        let r = DecideRequest {
            explain: true,
            ..req("Keep?")
        };
        let d = core.decide(&r, Door::Api).await.unwrap();
        assert!(d.state.unwrap().starts_with("Excerpt 1"));
        let d = core.decide(&req("Keep?"), Door::Api).await.unwrap();
        assert_eq!(d.state, None);
    }

    #[tokio::test]
    async fn events_arrive_retrieved_then_reads_then_done() {
        let mut core = three_base().await;
        deciding(&mut core, marked(0.9));
        let evs: Vec<DecideEvent> = core
            .decide_events(&req("Keep?"), Door::Api)
            .map(|e| e.unwrap())
            .collect()
            .await;
        assert_eq!(evs.len(), 5);
        assert!(matches!(evs[0], DecideEvent::Retrieved { passages: 3 }));
        let mut ranks: Vec<usize> = evs[1..4]
            .iter()
            .map(|e| match e {
                DecideEvent::Read(r) => {
                    assert_eq!(r.of, 3);
                    assert_eq!(r.bearing, r.bears >= 0.5);
                    r.rank
                }
                _ => panic!("{e:?}"),
            })
            .collect();
        ranks.sort();
        assert_eq!(ranks, vec![1, 2, 3]);
        assert!(matches!(evs[4], DecideEvent::Done(_)));
    }

    /// Wraps a rule with what a sync rule cannot do: probe the lane, or wait.
    struct Probe {
        inner: RuleDecider,
        gate: Arc<crate::infer::gate::InferenceGate>,
        background_was_held: AtomicBool,
        sleep_on_excerpts: bool,
        finished: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Decider for Probe {
        async fn decide(&self, state: &str, qs: &[Question]) -> crate::error::Result<Answers> {
            let got = self.inner.decide(state, qs).await;
            if self.sleep_on_excerpts && state.contains("Excerpt:") {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
            let held =
                tokio::time::timeout(std::time::Duration::from_millis(50), self.gate.background())
                    .await
                    .is_err();
            self.background_was_held.fetch_and(held, Ordering::SeqCst);
            self.finished.fetch_add(1, Ordering::SeqCst);
            got
        }
        fn context_tokens(&self) -> usize {
            self.inner.context_tokens()
        }
        fn model(&self) -> &str {
            "probe"
        }
    }

    fn probing(core: &mut Core, sleep_on_excerpts: bool) -> Arc<Probe> {
        let p = Arc::new(Probe {
            inner: RuleDecider::new(marked(0.9)),
            gate: Arc::clone(&core.gate),
            background_was_held: AtomicBool::new(true),
            sleep_on_excerpts,
            finished: AtomicUsize::new(0),
        });
        core.decider = p.clone();
        p
    }

    #[tokio::test]
    async fn a_decision_holds_the_interactive_lane() {
        let mut core = three_base().await;
        let p = probing(&mut core, false);
        core.decide(&req("Keep?"), Door::Api).await.unwrap();
        assert_eq!(p.finished.load(Ordering::SeqCst), 5);
        assert!(
            p.background_was_held.load(Ordering::SeqCst),
            "a window could have started during one of the decision's reads"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), core.gate.background())
            .await
            .expect("the interactive lane was never released");
    }

    #[tokio::test]
    async fn a_dropped_stream_stops_reading_and_frees_the_lane() {
        let mut core = three_base().await;
        let p = probing(&mut core, true);
        let s = core.decide_events(&req("Keep?"), Door::Api);
        let mut s = Box::pin(s);
        assert!(matches!(
            s.next().await,
            Some(Ok(DecideEvent::Retrieved { passages: 3 }))
        ));
        // Past retrieval nothing touches the store, so the clock may stop.
        tokio::time::pause();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), s.next())
                .await
                .is_err()
        );
        let started = p.inner.calls();
        assert_eq!(started, 4, "the prior and three reads in flight");
        drop(s);
        tokio::time::timeout(std::time::Duration::from_secs(5), core.gate.background())
            .await
            .expect("the interactive lane was never released");
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        assert_eq!(p.inner.calls(), started);
        assert_eq!(
            p.finished.load(Ordering::SeqCst),
            1,
            "only the prior finished"
        );
    }

    pub(crate) fn none_bear() -> impl Fn(&str, &[Question]) -> crate::error::Result<Answers> {
        |_, qs| {
            Ok(answers(qs, |q| Answer::Noul {
                yes: if q.key == "bears" { 0.02 } else { 0.99 },
            }))
        }
    }

    async fn learning() -> Core {
        let mut core = three_base().await;
        core.learn.enabled = true;
        core
    }

    async fn decide_gaps(core: &Core) -> Vec<crate::store::gaps::Gap> {
        core.store
            .open_gap_refs(core.embedder.model(), core.weak_below())
            .await
            .unwrap()
            .into_iter()
            .filter(|g| g.kind == crate::store::gaps::GapKind::Decide)
            .collect()
    }

    #[tokio::test]
    async fn a_decision_from_the_ui_is_recorded_with_its_evidence() {
        let mut core = learning().await;
        deciding(&mut core, marked(0.9));
        let r = DecideRequest {
            situation: Some("12 GB.".into()),
            limit: Some(5),
            ..req("  Keep bge-m3?  ")
        };
        let d = core.decide(&r, Door::Ui.by("user-1")).await.unwrap();
        let id = d.event_id.clone().expect("a UI decision is recorded");
        let s = core.store.decision(&id).await.unwrap().unwrap();
        assert_eq!(s.question, "Keep bge-m3?");
        assert_eq!(s.questions, wire::questions_json(&d.questions));
        assert!(!s.not_held);
        let standing: serde_json::Value = serde_json::from_str(&s.standing).unwrap();
        assert_eq!(standing, serde_json::json!({"verdict": "settled"}));
        let answers: serde_json::Value =
            serde_json::from_str(s.answers.as_deref().unwrap()).unwrap();
        assert_eq!(answers["verdict"]["noul"], 0.9);
        let row = sqlx::query!(
            "SELECT scope, filters, situation, prior, model, read, dropped, vec_dim, embed_model
               FROM decide_events WHERE id = ?",
            id
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(row.scope.as_deref(), Some("user-1"));
        let filters: serde_json::Value = serde_json::from_str(&row.filters).unwrap();
        assert_eq!(
            filters,
            serde_json::json!({"tags": [], "category": null, "limit": 5})
        );
        assert_eq!(row.situation.as_deref(), Some("12 GB."));
        assert!(row.prior.contains("\"verdict\""));
        assert_eq!((row.model.as_str(), row.read, row.dropped), ("rule", 3, 0));
        assert!(row.vec_dim > 0);
        assert_eq!(row.embed_model, core.embedder.model());

        assert_eq!(s.evidence.len(), 3, "every passage read, bearing or not");
        assert_eq!(
            s.evidence.iter().map(|e| e.rank).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        for e in &s.evidence {
            let r = &d.reads[e.rank as usize - 1];
            assert_eq!(e.artifact_id, r.artifact_id);
            assert!((e.bears - r.bears).abs() < 1e-9);
            assert_eq!(e.bearing, r.bearing);
            let placed = d.evidence.iter().find(|x| x.artifact_id == e.artifact_id);
            assert_eq!(e.n, placed.map(|x| x.n as i64));
            assert_eq!(e.joint, placed.is_some_and(|x| x.joint));
            let read: serde_json::Value = serde_json::from_str(&e.read).unwrap();
            assert_eq!(read["verdict"]["type"], "noul");
        }
        assert_eq!(s.evidence.iter().filter(|e| e.bearing).count(), 2);
    }

    #[tokio::test]
    async fn the_recorded_limit_is_the_one_asked_for() {
        let mut core = learning().await;
        deciding(&mut core, marked(0.9));
        for (limit, kept) in [(Some(40), 40), (None, DEFAULT_LIMIT as i64)] {
            let r = DecideRequest {
                limit,
                ..req("Keep bge-m3?")
            };
            let id = core
                .decide(&r, Door::Ui.by("me"))
                .await
                .unwrap()
                .event_id
                .unwrap();
            let filters: String =
                sqlx::query_scalar!("SELECT filters FROM decide_events WHERE id = ?", id)
                    .fetch_one(&core.store.pool)
                    .await
                    .unwrap();
            let filters: serde_json::Value = serde_json::from_str(&filters).unwrap();
            assert_eq!(filters["limit"], kept, "as ask stores it, not clamped");
        }
    }

    #[tokio::test]
    async fn a_held_decision_marks_what_it_was_weighed_from_as_cited() {
        let mut core = learning().await;
        deciding(&mut core, marked(0.9));
        let d = core
            .decide(&req("Keep bge-m3?"), Door::Ui.by("me"))
            .await
            .unwrap();
        core.background.wait_idle().await;
        let ids: Vec<String> = d.reads.iter().map(|r| r.artifact_id.clone()).collect();
        let act = core.store.activation_of(&ids).await.unwrap();
        let of = |id: &str| act.get(id).map(|(a, _, _)| *a).unwrap_or(0.0);
        let unread = of(&d.reads.iter().find(|r| !r.bearing).unwrap().artifact_id);
        assert_eq!(d.evidence.len(), 2);
        for e in &d.evidence {
            assert!(
                (of(&e.artifact_id) - unread - core.activation.cited).abs() < 1e-6,
                "cited {} against {unread}",
                of(&e.artifact_id)
            );
        }

        let mut core = learning().await;
        deciding(&mut core, none_bear());
        let d = core
            .decide(&req("Keep bge-m3?"), Door::Ui.by("me"))
            .await
            .unwrap();
        core.background.wait_idle().await;
        assert!(d.not_held() && d.event_id.is_some());
        let ids: Vec<String> = d.reads.iter().map(|r| r.artifact_id.clone()).collect();
        let act = core.store.activation_of(&ids).await.unwrap();
        let first = act.get(&ids[0]).map(|(a, _, _)| *a);
        assert!(
            ids.iter().all(|i| act.get(i).map(|(a, _, _)| *a) == first),
            "a decision the base does not hold cites nothing"
        );
    }

    #[tokio::test]
    async fn the_api_and_mcp_doors_leave_no_record() {
        let mut core = learning().await;
        deciding(&mut core, marked(0.9));
        for door in [Door::Api, Door::Mcp] {
            let d = core.decide(&req("Keep bge-m3?"), door).await.unwrap();
            assert_eq!(d.event_id, None);
        }
        for door in [Door::Cli, Door::App] {
            let d = core.decide(&req("Keep bge-m3?"), door).await.unwrap();
            assert!(d.event_id.is_some(), "{door:?}");
        }
        let n: i64 = sqlx::query_scalar!("SELECT count(*) FROM decide_events")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn nothing_is_recorded_while_learning_is_off() {
        let mut core = three_base().await;
        core.learn.enabled = false;
        deciding(&mut core, marked(0.9));
        let d = core
            .decide(&req("Keep bge-m3?"), Door::Ui.by("me"))
            .await
            .unwrap();
        assert_eq!(d.event_id, None);
        let n: i64 = sqlx::query_scalar!("SELECT count(*) FROM decide_events")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn a_not_held_decision_is_recorded_and_is_an_open_gap() {
        let mut core = learning().await;
        deciding(&mut core, none_bear());
        let d = core
            .decide(&req("Keep bge-m3?"), Door::Ui.by("me"))
            .await
            .unwrap();
        let id = d.event_id.clone().expect("recorded");
        let s = core.store.decision(&id).await.unwrap().unwrap();
        assert!(s.not_held && s.answers.is_none());
        assert_eq!(s.evidence.len(), 3);
        assert!(
            s.evidence
                .iter()
                .all(|e| !e.bearing && e.n.is_none() && !e.joint)
        );
        let gaps = decide_gaps(&core).await;
        assert_eq!(gaps.len(), 1);
        assert_eq!(
            (gaps[0].id.as_str(), gaps[0].text.as_str()),
            (id.as_str(), "Keep bge-m3?")
        );
        let open = core
            .store
            .open_gaps(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        assert!(
            open.gaps
                .iter()
                .any(|g| g.gap.kind == crate::store::gaps::GapKind::Decide && !g.vec.is_empty())
        );
        let n = core
            .store
            .count_open_gaps(core.embedder.model(), core.weak_below())
            .await
            .unwrap();
        assert_eq!(
            n as usize,
            core.store
                .open_gap_refs(core.embedder.model(), core.weak_below())
                .await
                .unwrap()
                .len()
        );
    }

    #[tokio::test]
    async fn a_held_decision_judged_nothing_here_becomes_a_gap_and_dismissing_closes_it() {
        use crate::store::asks::AskVerdict;
        let mut core = learning().await;
        deciding(&mut core, marked(0.9));
        let d = core
            .decide(&req("Keep bge-m3?"), Door::Ui.by("me"))
            .await
            .unwrap();
        let id = d.event_id.unwrap();
        assert!(
            decide_gaps(&core).await.is_empty(),
            "a held decision is no gap"
        );
        core.store
            .judge_decision(&id, AskVerdict::NothingHere, None)
            .await
            .unwrap();
        assert_eq!(decide_gaps(&core).await.len(), 1);
        core.store
            .dismiss_gap(crate::store::gaps::GapKind::Decide, &id)
            .await
            .unwrap();
        assert!(decide_gaps(&core).await.is_empty());
        assert!(matches!(
            core.store
                .dismiss_gap(crate::store::gaps::GapKind::Decide, "nope")
                .await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_not_held_decision_judged_wrong_is_no_gap_and_undoing_reopens_it() {
        use crate::store::asks::AskVerdict;
        let mut core = learning().await;
        deciding(&mut core, none_bear());
        let d = core
            .decide(&req("Keep bge-m3?"), Door::Ui.by("me"))
            .await
            .unwrap();
        let id = d.event_id.unwrap();
        assert_eq!(decide_gaps(&core).await.len(), 1);
        core.store
            .judge_decision(&id, AskVerdict::Wrong, None)
            .await
            .unwrap();
        assert!(decide_gaps(&core).await.is_empty());
        core.store.unjudge_decision(&id).await.unwrap();
        assert_eq!(decide_gaps(&core).await.len(), 1);
    }
}
