//! Decide's JSON: the request read in the caller's order, the response written in it.

use super::{DecideRequest, Decision, Evidence, Standing, refuse};
use crate::error::Result;
use crate::infer::decide::{Answer, Kind, Question};
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use std::fmt;
use std::marker::PhantomData;

/// System One's shape, so a stored decision reads back as the questions the model was asked.
pub use crate::infer::decide::wire::questions_json;

/// The request body, in the caller's order.
#[derive(Debug, serde::Deserialize)]
pub struct DecideBody {
    q: String,
    #[serde(default)]
    questions: Option<InOrder<RawQuestion>>,
    #[serde(default)]
    situation: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    explain: bool,
}

/// An object's entries as they came: option order is meaning, and a map would sort it.
#[derive(Debug)]
struct InOrder<T>(Vec<(String, T)>);

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for InOrder<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: serde::Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = InOrder<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(entry) = m.next_entry::<String, T>()? {
                    out.push(entry);
                }
                Ok(InOrder(out))
            }
        }
        d.deserialize_map(V(PhantomData))
    }
}

/// Read as either shape, so the mismatch is named by `into_request` and not by serde.
#[derive(Debug)]
enum Criteria {
    Options(Vec<(String, String)>),
    Levels(Vec<String>),
}

impl<'de> serde::Deserialize<'de> for Criteria {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Criteria;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of option to description, or an array of levels")
            }
            fn visit_map<A: MapAccess<'de>>(self, m: A) -> std::result::Result<Criteria, A::Error> {
                let o: InOrder<String> =
                    serde::Deserialize::deserialize(de::value::MapAccessDeserializer::new(m))?;
                Ok(Criteria::Options(o.0))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut s: A,
            ) -> std::result::Result<Criteria, A::Error> {
                let mut out = Vec::new();
                while let Some(l) = s.next_element::<String>()? {
                    out.push(l);
                }
                Ok(Criteria::Levels(out))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Debug, serde::Deserialize)]
struct RawQuestion {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    criteria: Option<Criteria>,
}

impl RawQuestion {
    fn into_question(self, key: String) -> Result<Question> {
        let kind = match (self.kind.as_str(), self.criteria) {
            ("choice", Some(Criteria::Options(options))) => Kind::Choice { options },
            ("score", Some(Criteria::Levels(levels))) => Kind::Score { levels },
            ("noul", None) => Kind::Noul,
            ("choice", _) => {
                return Err(refuse(&format!(
                    "\"{key}\" is a choice; its criteria are an object of option to description."
                )));
            }
            ("score", _) => {
                return Err(refuse(&format!(
                    "\"{key}\" is a score; its criteria are an array of levels, lowest first."
                )));
            }
            ("noul", _) => {
                return Err(refuse(&format!(
                    "\"{key}\" is a noul and takes no criteria."
                )));
            }
            (other, _) => {
                return Err(refuse(&format!(
                    "\"{key}\" has the type \"{other}\"; a question is a choice, a score or a noul."
                )));
            }
        };
        Ok(Question {
            key,
            kind,
            instructions: self.instructions.unwrap_or_default(),
        })
    }
}

impl DecideBody {
    /// `Error::Validation` for a question whose shape does not match its type.
    pub fn into_request(self) -> Result<DecideRequest> {
        let questions = self
            .questions
            .map(|q| q.0)
            .unwrap_or_default()
            .into_iter()
            .map(|(key, raw)| raw.into_question(key))
            .collect::<Result<Vec<_>>>()?;
        Ok(DecideRequest {
            q: self.q,
            questions,
            situation: self.situation,
            limit: self.limit,
            tags: self.tags,
            category: self.category,
            explain: self.explain,
        })
    }
}

/// A stored decision's questions, read back from the shape `questions_json` wrote.
pub(crate) fn stored_questions(text: &str) -> Result<Vec<Question>> {
    serde_json::from_str::<InOrder<RawQuestion>>(text)
        .map_err(|e| crate::error::Error::Internal(format!("a stored decision's questions: {e}")))?
        .0
        .into_iter()
        .map(|(key, raw)| raw.into_question(key))
        .collect()
}

/// `q`'s answer in a stored `{key: answer}`, as `AnswerOut` wrote it; `None` when it does not read.
pub(crate) fn stored_answer(q: &Question, text: &str) -> Option<Answer> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    answer_from(q, v.get(&q.key)?)
}

/// One answer as `AnswerOut` writes it, in the response and in the store alike.
pub(crate) fn answer_from(q: &Question, a: &serde_json::Value) -> Option<Answer> {
    let f = |k: &str| a.get(k).and_then(serde_json::Value::as_f64);
    Some(match &q.kind {
        Kind::Choice { options } => {
            let table = a.get("probabilities")?;
            Answer::Choice {
                pick: a.get("choice")?.as_str()?.to_string(),
                probabilities: options
                    .iter()
                    .map(|(n, _)| table.get(n).and_then(serde_json::Value::as_f64))
                    .collect::<Option<_>>()?,
                confidence: f("confidence")?,
            }
        }
        Kind::Score { .. } => Answer::Score {
            score: f("score")?,
            probabilities: a
                .get("probabilities")?
                .as_array()?
                .iter()
                .map(serde_json::Value::as_f64)
                .collect::<Option<_>>()?,
            confidence: f("confidence")?,
        },
        Kind::Noul => Answer::Noul { yes: f("noul")? },
    })
}

/// One answer as the response writes it; a choice's probabilities by name, in option order.
struct AnswerOut<'a> {
    options: &'a [(String, String)],
    answer: &'a Answer,
    swapped_agree: Option<bool>,
}

impl<'a> AnswerOut<'a> {
    fn new(q: Option<&'a Question>, answer: &'a Answer, swapped_agree: Option<bool>) -> Self {
        let options = match q.map(|q| &q.kind) {
            Some(Kind::Choice { options }) => options.as_slice(),
            _ => &[],
        };
        AnswerOut {
            options,
            answer,
            swapped_agree,
        }
    }
}

struct Named<'a>(&'a [(String, String)], &'a [f64]);

impl serde::Serialize for Named<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.1.len()))?;
        for (i, p) in self.1.iter().enumerate() {
            match self.0.get(i) {
                Some((name, _)) => m.serialize_entry(name, p)?,
                // An answer is checked against its question, so this is only a floor.
                None => m.serialize_entry(&i.to_string(), p)?,
            }
        }
        m.end()
    }
}

impl serde::Serialize for AnswerOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        match self.answer {
            Answer::Choice {
                pick,
                probabilities,
                confidence,
            } => {
                m.serialize_entry("type", "choice")?;
                m.serialize_entry("choice", pick)?;
                m.serialize_entry("probabilities", &Named(self.options, probabilities))?;
                m.serialize_entry("confidence", confidence)?;
            }
            Answer::Score {
                score,
                probabilities,
                confidence,
            } => {
                m.serialize_entry("type", "score")?;
                m.serialize_entry("score", score)?;
                m.serialize_entry("probabilities", probabilities)?;
                m.serialize_entry("confidence", confidence)?;
            }
            Answer::Noul { yes } => {
                m.serialize_entry("type", "noul")?;
                m.serialize_entry("noul", yes)?;
            }
        }
        if let Some(agree) = self.swapped_agree {
            m.serialize_entry("swapped_agree", &agree)?;
        }
        m.end()
    }
}

/// For storage and display; never used to order a choice, which `Serialize for Decision` does itself.
pub fn answer_json(q: &Question, a: &Answer, swapped_agree: Option<bool>) -> serde_json::Value {
    serde_json::to_value(AnswerOut::new(Some(q), a, swapped_agree)).unwrap_or_default()
}

/// `{key: answer}` in question order.
struct Answers<'a> {
    questions: &'a [Question],
    rows: Vec<(&'a str, &'a Answer, Option<bool>)>,
}

impl serde::Serialize for Answers<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.rows.len()))?;
        for (k, a, agree) in &self.rows {
            let q = self.questions.iter().find(|q| q.key == *k);
            m.serialize_entry(k, &AnswerOut::new(q, a, *agree))?;
        }
        m.end()
    }
}

fn plain<'a>(questions: &'a [Question], read: &'a [(String, Answer)]) -> Answers<'a> {
    Answers {
        questions,
        rows: read.iter().map(|(k, a)| (k.as_str(), a, None)).collect(),
    }
}

fn held(d: &Decision) -> Option<Answers<'_>> {
    d.answers.as_ref().map(|v| Answers {
        questions: &d.questions,
        rows: v
            .iter()
            .map(|(k, v)| (k.as_str(), &v.answer, v.swapped_agree))
            .collect(),
    })
}

struct Standings<'a>(&'a [(String, Standing)]);

impl serde::Serialize for Standings<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, st) in self.0 {
            m.serialize_entry(k, st.as_str())?;
        }
        m.end()
    }
}

struct EvidenceOut<'a>(&'a [Question], &'a Evidence);

impl serde::Serialize for EvidenceOut<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let e = self.1;
        let mut m = s.serialize_map(Some(7))?;
        m.serialize_entry("n", &e.n)?;
        m.serialize_entry("artifact_id", &e.artifact_id)?;
        m.serialize_entry("title", &e.title)?;
        m.serialize_entry("text", &*e.text)?;
        m.serialize_entry("bears", &e.bears)?;
        m.serialize_entry("joint", &e.joint)?;
        m.serialize_entry("read", &plain(self.0, &e.read))?;
        m.end()
    }
}

struct EvidenceList<'a>(&'a [Question], &'a [Evidence]);

impl serde::Serialize for EvidenceList<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_seq(self.1.iter().map(|e| EvidenceOut(self.0, e)))
    }
}

/// The spec's response; `reads` and `questions` are stored, not sent.
impl serde::Serialize for Decision {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("answers", &held(self))?;
        m.serialize_entry("prior", &plain(&self.questions, &self.prior))?;
        m.serialize_entry("standing", &Standings(&self.standing))?;
        m.serialize_entry("evidence", &EvidenceList(&self.questions, &self.evidence))?;
        m.serialize_entry("read", &self.read)?;
        m.serialize_entry("dropped", &self.dropped)?;
        m.serialize_entry("model", &self.model)?;
        if let Some(id) = &self.event_id {
            m.serialize_entry("event_id", id)?;
        }
        if let Some(state) = &self.state {
            m.serialize_entry("state", state)?;
        }
        m.end()
    }
}

/// What `decide_events` and `decide_evidence` store, in the response's own shapes.
pub(super) fn answers_text(d: &Decision) -> Option<String> {
    held(d).map(|a| to_text(&a))
}

pub(super) fn prior_text(d: &Decision) -> String {
    to_text(&plain(&d.questions, &d.prior))
}

pub(super) fn standing_text(d: &Decision) -> String {
    to_text(&Standings(&d.standing))
}

pub(super) fn read_text(questions: &[Question], read: &[(String, Answer)]) -> String {
    to_text(&plain(questions, read))
}

/// A stream's `read` frame, in the shape `done` gives each evidence's `read`.
pub fn read_json(questions: &[Question], read: &[(String, Answer)]) -> serde_json::Value {
    serde_json::to_value(plain(questions, read)).unwrap_or_default()
}

fn to_text(v: &impl serde::Serialize) -> String {
    // Maps of string keys and finite numbers: writing one to a string does not fail.
    serde_json::to_string(v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::decide::{Decision, Evidence, Standing, Verdict};
    use crate::error::Error;
    use crate::infer::decide::{Answer, Kind, Question};
    use serde_json::Value;

    fn body(s: &str) -> crate::error::Result<DecideRequest> {
        serde_json::from_str::<DecideBody>(s)
            .unwrap()
            .into_request()
    }

    #[test]
    fn criteria_keep_the_callers_order() {
        let r = body(
            r#"{"q":"x","questions":{"verdict":{"type":"choice","criteria":{"switch":"Move","keep":"Stay"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            r.questions[0].kind,
            Kind::Choice {
                options: vec![
                    ("switch".into(), "Move".into()),
                    ("keep".into(), "Stay".into())
                ]
            }
        );
    }

    #[test]
    fn questions_keep_the_callers_order() {
        let r = body(
            r#"{"q":"x","questions":{"zeta":{"type":"noul"},"alpha":{"type":"noul","instructions":"A?"}}}"#,
        )
        .unwrap();
        let keys: Vec<&str> = r.questions.iter().map(|q| q.key.as_str()).collect();
        assert_eq!(keys, vec!["zeta", "alpha"]);
        assert_eq!(r.questions[0].instructions, "");
        assert_eq!(r.questions[1].instructions, "A?");
    }

    #[test]
    fn an_absent_questions_field_is_the_default_noul() {
        let r = body(r#"{"q":"x","tags":["a"],"limit":4,"explain":true,"situation":"s"}"#).unwrap();
        assert!(r.questions.is_empty());
        assert_eq!(r.tags, vec!["a".to_string()]);
        assert_eq!((r.limit, r.explain), (Some(4), true));
        assert_eq!(r.situation.as_deref(), Some("s"));
        assert!(
            body(r#"{"q":"x","questions":null}"#)
                .unwrap()
                .questions
                .is_empty()
        );
    }

    #[test]
    fn a_score_takes_an_array_and_a_noul_takes_nothing() {
        let r = body(
            r#"{"q":"x","questions":{"urgency":{"type":"score","criteria":["later","now"]},"yes":{"type":"noul"}}}"#,
        )
        .unwrap();
        assert_eq!(
            r.questions[0].kind,
            Kind::Score {
                levels: vec!["later".into(), "now".into()]
            }
        );
        assert_eq!(r.questions[1].kind, Kind::Noul);
    }

    #[test]
    fn a_shape_that_does_not_match_its_type_names_the_key() {
        for bad in [
            r#"{"q":"x","questions":{"verdict":{"type":"choice","criteria":["a","b"]}}}"#,
            r#"{"q":"x","questions":{"verdict":{"type":"score","criteria":{"a":"A"}}}}"#,
            r#"{"q":"x","questions":{"verdict":{"type":"noul","criteria":["a","b"]}}}"#,
            r#"{"q":"x","questions":{"verdict":{"type":"choice"}}}"#,
            r#"{"q":"x","questions":{"verdict":{"type":"vote","criteria":["a","b"]}}}"#,
        ] {
            match body(bad) {
                Err(Error::Validation(m)) => assert!(m.contains("verdict"), "{m}"),
                other => panic!("{bad}: {other:?}"),
            }
        }
    }

    fn keep_switch() -> Question {
        Question {
            key: "verdict".into(),
            kind: Kind::Choice {
                options: vec![
                    ("keep".into(), "Stay".into()),
                    ("switch".into(), "Move".into()),
                ],
            },
            instructions: "Which?".into(),
        }
    }

    fn choice(pick: &str, p: [f64; 2], c: f64) -> Answer {
        Answer::Choice {
            pick: pick.into(),
            probabilities: p.to_vec(),
            confidence: c,
        }
    }

    fn held() -> Decision {
        Decision {
            question: "Keep?".into(),
            questions: vec![keep_switch()],
            answers: Some(vec![(
                "verdict".into(),
                Verdict {
                    answer: choice("keep", [0.81, 0.19], 0.62),
                    swapped_agree: Some(true),
                },
            )]),
            prior: vec![("verdict".into(), choice("keep", [0.55, 0.45], 0.07))],
            standing: vec![("verdict".into(), Standing::Settled)],
            evidence: vec![Evidence {
                n: 1,
                artifact_id: "art-1".into(),
                title: Some("Sizing".into()),
                text: "Qdrant sizing is fine.".into(),
                bears: 0.93,
                joint: true,
                read: vec![("verdict".into(), choice("keep", [0.9, 0.1], 0.8))],
            }],
            read: 8,
            dropped: 0,
            model: "clef".into(),
            event_id: None,
            state: None,
            reads: vec![],
        }
    }

    #[test]
    fn the_response_is_the_specs_shape() {
        // Probabilities in option order even where the names sort the other way.
        let mut d = held();
        if let Some(Kind::Choice { options }) = d.questions.first_mut().map(|q| &mut q.kind) {
            options.reverse();
        }
        let flip = |a: &mut Answer| {
            if let Answer::Choice { probabilities, .. } = a {
                probabilities.reverse();
            }
        };
        flip(&mut d.answers.as_mut().unwrap()[0].1.answer);
        flip(&mut d.prior[0].1);
        flip(&mut d.evidence[0].read[0].1);
        let text = serde_json::to_string(&d).unwrap();
        assert!(
            text.contains(r#""probabilities":{"switch":0.19,"keep":0.81}"#),
            "{text}"
        );
        let text = serde_json::to_string(&held()).unwrap();
        assert!(
            text.contains(r#""probabilities":{"keep":0.81,"switch":0.19}"#),
            "{text}"
        );
        let v: Value = serde_json::from_str(&text).unwrap();
        let a = &v["answers"]["verdict"];
        assert_eq!(a["type"], "choice");
        assert_eq!(a["choice"], "keep");
        assert_eq!(a["confidence"], 0.62);
        assert_eq!(a["swapped_agree"], true);
        assert!(v["prior"]["verdict"].get("swapped_agree").is_none());
        assert_eq!(v["prior"]["verdict"]["choice"], "keep");
        assert_eq!(v["standing"]["verdict"], "settled");
        let e = &v["evidence"][0];
        assert_eq!(e["n"], 1);
        assert_eq!(e["artifact_id"], "art-1");
        assert_eq!(e["title"], "Sizing");
        assert_eq!(e["text"], "Qdrant sizing is fine.");
        assert_eq!(e["bears"], 0.93);
        assert_eq!(e["joint"], true);
        assert_eq!(e["read"]["verdict"]["choice"], "keep");
        assert!(e["read"]["verdict"].get("swapped_agree").is_none());
        assert_eq!(
            (v["read"].as_u64(), v["dropped"].as_u64()),
            (Some(8), Some(0))
        );
        assert_eq!(v["model"], "clef");
        assert!(v.get("event_id").is_none() && v.get("state").is_none());
        assert!(v.get("reads").is_none());

        let mut d = held();
        d.event_id = Some("ev-1".into());
        d.state = Some("Excerpt 1".into());
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(
            (v["event_id"].as_str(), v["state"].as_str()),
            (Some("ev-1"), Some("Excerpt 1"))
        );
    }

    #[test]
    fn a_not_held_response_has_null_answers_and_empty_evidence() {
        let mut d = held();
        d.answers = None;
        d.evidence.clear();
        d.standing = vec![("verdict".into(), Standing::NotHeld)];
        let text = serde_json::to_string(&d).unwrap();
        assert!(text.contains(r#""answers":null"#), "{text}");
        assert!(text.contains(r#""evidence":[]"#), "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["standing"]["verdict"], "not_held");
        assert_eq!(v["prior"]["verdict"]["type"], "choice");
    }

    #[test]
    fn noul_and_score_answers_have_their_own_shapes() {
        let noul = Question::noul("verdict", "Is it?");
        assert_eq!(
            answer_json(&noul, &Answer::Noul { yes: 0.9 }, None),
            serde_json::json!({"type": "noul", "noul": 0.9})
        );
        let score = Question {
            key: "urgency".into(),
            kind: Kind::Score {
                levels: vec!["later".into(), "soon".into(), "now".into()],
            },
            instructions: "How urgent?".into(),
        };
        let a = Answer::Score {
            score: 1.2,
            probabilities: vec![0.1, 0.6, 0.3],
            confidence: 0.4,
        };
        assert_eq!(
            answer_json(&score, &a, None),
            serde_json::json!({"type": "score", "score": 1.2,
                               "probabilities": [0.1, 0.6, 0.3], "confidence": 0.4})
        );
        let c = answer_json(
            &keep_switch(),
            &choice("keep", [0.81, 0.19], 0.62),
            Some(false),
        );
        assert_eq!(c["swapped_agree"], false);
        assert_eq!(c["probabilities"]["switch"], 0.19);
    }

    #[test]
    fn the_stored_questions_are_system_ones_shape() {
        let s = questions_json(&[keep_switch()]);
        assert_eq!(
            s,
            r#"{"verdict":{"type":"choice","instructions":"Which?","criteria":{"keep":"Stay","switch":"Move"}}}"#
        );
    }
}
