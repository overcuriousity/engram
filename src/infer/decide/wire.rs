//! System One's request and response, byte-compatible with magicshell (`tests/fixtures/systemone`).

use super::{Answer, Answers, Kind, Question, trim};
use crate::error::{Error, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;

fn json_str(s: &str) -> String {
    Value::String(s.to_owned()).to_string()
}

fn question_json(q: &Question) -> String {
    let (kind, criteria) = match &q.kind {
        Kind::Choice { options } => {
            // Entry by entry: a JSON object model would reorder integer-like names.
            let entries: Vec<String> = options
                .iter()
                .map(|(name, description)| {
                    let (name, description) = (trim(name), trim(description));
                    let description = if description.is_empty() {
                        name
                    } else {
                        description
                    };
                    format!("{}:{}", json_str(name), json_str(description))
                })
                .collect();
            ("choice", format!(",\"criteria\":{{{}}}", entries.join(",")))
        }
        Kind::Score { levels } => {
            let levels: Vec<String> = levels.iter().map(|l| json_str(trim(l))).collect();
            ("score", format!(",\"criteria\":[{}]", levels.join(",")))
        }
        Kind::Noul => ("noul", String::new()),
    };
    format!(
        "{}:{{\"type\":{},\"instructions\":{}{criteria}}}",
        json_str(&q.key),
        json_str(kind),
        json_str(trim(&q.instructions))
    )
}

/// The `questions` object alone, as it appears in the body; what `budget` counts.
pub fn questions_json(questions: &[Question]) -> String {
    let entries: Vec<String> = questions.iter().map(question_json).collect();
    format!("{{{}}}", entries.join(","))
}

/// Compact; keys in the order model, state, questions; criteria entry by entry.
pub fn request_json(model: &str, state: &str, questions: &[Question]) -> String {
    format!(
        "{{\"model\":{},\"state\":{},\"questions\":{}}}",
        json_str(model),
        json_str(state),
        questions_json(questions)
    )
}

fn bad(what: &str) -> Error {
    Error::InferenceRejected {
        role: "decide",
        detail: format!("the answer {what}"),
    }
}

fn number(v: Option<&Value>, what: &str) -> Result<f64> {
    v.and_then(Value::as_f64)
        .filter(|f| f.is_finite())
        .ok_or_else(|| bad(&format!("has no number for {what}")))
}

fn unit(v: Option<&Value>, what: &str) -> Result<f64> {
    let f = number(v, what)?;
    if (0.0..=1.0).contains(&f) {
        Ok(f)
    } else {
        Err(bad(&format!("has {what} outside 0 to 1")))
    }
}

fn probabilities(a: &Map<String, Value>, names: &[String]) -> Result<Vec<f64>> {
    let Some(Value::Object(table)) = a.get("probabilities") else {
        return Err(bad("has no probabilities"));
    };
    names
        .iter()
        .map(|n| unit(table.get(n), &format!("probability \"{n}\"")))
        .collect()
}

/// `body` is the response text. Probabilities are put in option or level order; the endpoint sorts a choice's by name.
pub fn parse_response(body: &str, questions: &[Question]) -> Result<Answers> {
    let Ok(Value::Object(body)) = serde_json::from_str::<Value>(body) else {
        return Err(bad("is not a JSON object"));
    };
    let Some(Value::Object(upstream)) = body.get("answers") else {
        return Err(bad("has no answers"));
    };
    let mut answers = HashMap::new();
    for q in questions {
        let Some(Value::Object(a)) = upstream.get(&q.key) else {
            return Err(bad(&format!("lacks question \"{}\"", q.key)));
        };
        let kind = match &q.kind {
            Kind::Choice { .. } => "choice",
            Kind::Score { .. } => "score",
            Kind::Noul => "noul",
        };
        if a.get("type").and_then(Value::as_str) != Some(kind) {
            return Err(bad(&format!("has the wrong type for \"{}\"", q.key)));
        }
        let answer = match &q.kind {
            Kind::Choice { options } => {
                let names: Vec<String> = options.iter().map(|(n, _)| trim(n).to_owned()).collect();
                let pick = match a.get("choice") {
                    Some(Value::String(c)) if names.contains(c) => c.clone(),
                    _ => return Err(bad(&format!("picks no option of \"{}\"", q.key))),
                };
                Answer::Choice {
                    pick,
                    probabilities: probabilities(a, &names)?,
                    confidence: number(a.get("confidence"), "confidence")?,
                }
            }
            Kind::Score { levels } => {
                let indexes: Vec<String> = (0..levels.len()).map(|i| i.to_string()).collect();
                Answer::Score {
                    score: number(a.get("score"), "score")?,
                    probabilities: probabilities(a, &indexes)?,
                    confidence: number(a.get("confidence"), "confidence")?,
                }
            }
            Kind::Noul => Answer::Noul {
                yes: unit(a.get("noul"), "noul")?,
            },
        };
        answers.insert(q.key.clone(), answer);
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let input_tokens = body
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_number)
        .and_then(|n| {
            n.as_u64()
                .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
        })
        .unwrap_or(0);
    Ok(Answers {
        answers,
        model,
        input_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/systemone");

    fn fixtures(prefix: &str) -> Vec<Value> {
        let mut paths: Vec<_> = std::fs::read_dir(DIR)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy();
                n.starts_with(prefix) && n.ends_with(".json")
            })
            .collect();
        paths.sort();
        paths
            .iter()
            .map(|p| serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap())
            .collect()
    }

    fn text(v: &Value) -> String {
        v.as_str().unwrap_or_default().to_owned()
    }

    fn run_of(fixture: &Value) -> (String, Vec<Question>) {
        let run = &fixture["run"];
        let questions = run["questions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| {
                let kind = match q["type"].as_str().unwrap() {
                    "choice" => Kind::Choice {
                        options: q["options"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|o| (text(&o["name"]), text(&o["description"])))
                            .collect(),
                    },
                    "score" => Kind::Score {
                        levels: q["levels"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|l| text(&l["text"]))
                            .collect(),
                    },
                    _ => Kind::Noul,
                };
                Question {
                    key: text(&q["key"]),
                    kind,
                    instructions: text(&q["text"]),
                }
            })
            .collect();
        (text(&run["state"]), questions)
    }

    #[test]
    fn every_request_fixture_is_reproduced_byte_for_byte() {
        let all = fixtures("request-");
        assert!(all.len() >= 7, "{}", all.len());
        for f in &all {
            let (state, qs) = run_of(f);
            assert_eq!(
                request_json(f["model"].as_str().unwrap(), &state, &qs),
                f["expected_json"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn every_response_fixture_reads_as_expected() {
        let all = fixtures("response-");
        assert!(all.len() >= 10, "{}", all.len());
        for f in &all {
            let (_, qs) = run_of(f);
            let body = match &f["body"] {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            };
            let expected = &f["expected"];
            let got = parse_response(&body, &qs);
            if expected.get("error").is_some() {
                assert!(
                    matches!(got, Err(Error::InferenceRejected { role: "decide", .. })),
                    "{f}"
                );
                continue;
            }
            let a = got.unwrap_or_else(|e| panic!("{e}: {f}"));
            assert_eq!(a.model, expected["model"].as_str().unwrap());
            assert_eq!(a.input_tokens, expected["inputTokens"].as_u64().unwrap());
            for (key, want) in expected["answers"].as_object().unwrap() {
                let floats = |v: &Value| {
                    v.as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_f64().unwrap())
                        .collect::<Vec<_>>()
                };
                match &a.answers[key] {
                    Answer::Choice {
                        pick,
                        probabilities,
                        confidence,
                    } => {
                        assert_eq!(pick, want["choice"].as_str().unwrap());
                        assert_eq!(*probabilities, floats(&want["probabilities"]));
                        assert_eq!(*confidence, want["confidence"].as_f64().unwrap());
                    }
                    Answer::Score {
                        score,
                        probabilities,
                        confidence,
                    } => {
                        assert_eq!(*score, want["score"].as_f64().unwrap());
                        assert_eq!(*probabilities, floats(&want["probabilities"]));
                        assert_eq!(*confidence, want["confidence"].as_f64().unwrap());
                    }
                    Answer::Noul { yes } => assert_eq!(*yes, want["noul"].as_f64().unwrap()),
                }
            }
        }
    }

    #[test]
    fn the_body_survives_awkward_text() {
        let q = Question {
            key: "v".into(),
            instructions: "Does «this» say \"yes\"?\nReally?".into(),
            kind: Kind::Choice {
                options: vec![
                    ("ja – klar".into(), "tab\there".into()),
                    ("nein".into(), String::new()),
                ],
            },
        };
        let v: Value =
            serde_json::from_str(&request_json("m", "Zustand: \"ß\"\n\u{2028}", &[q])).unwrap();
        assert_eq!(v["state"], "Zustand: \"ß\"\n\u{2028}");
        assert_eq!(
            v["questions"]["v"]["instructions"],
            "Does «this» say \"yes\"?\nReally?"
        );
        assert_eq!(v["questions"]["v"]["criteria"]["ja – klar"], "tab\there");
        assert_eq!(v["questions"]["v"]["criteria"]["nein"], "nein");
    }

    #[test]
    fn a_probability_outside_zero_to_one_is_refused() {
        let noul = [Question::noul("n", "Is it?")];
        let choice = [Question {
            key: "c".into(),
            instructions: "Which?".into(),
            kind: Kind::Choice {
                options: vec![("a".into(), String::new()), ("b".into(), String::new())],
            },
        }];
        for (qs, body) in [
            (
                &noul,
                json!({"answers": {"n": {"type": "noul", "noul": 1.2}}}),
            ),
            (
                &noul,
                json!({"answers": {"n": {"type": "noul", "noul": -0.1}}}),
            ),
            (
                &choice,
                json!({"answers": {"c": {"type": "choice", "choice": "a", "probabilities": {"a": 1.5, "b": 0.1}, "confidence": 0.5}}}),
            ),
        ] {
            let got = parse_response(&body.to_string(), qs);
            assert!(
                matches!(got, Err(Error::InferenceRejected { role: "decide", .. })),
                "{body}"
            );
        }
    }

    #[test]
    fn the_state_is_sent_untrimmed() {
        let v: Value = serde_json::from_str(&request_json(
            "m",
            "  x \n",
            &[Question::noul("n", "Is it?")],
        ))
        .unwrap();
        assert_eq!(v["state"], "  x \n");
    }
}
