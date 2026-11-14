//! `-d`: one typed question, decided from the notes, printed as weights and what they rest on.

use crate::cli::args::{CliArgs, DecideKind};
use crate::cli::endpoint::{Endpoint, client};
use crate::error::{Error, Result};
use serde_json::{Map, Value, json};

const CELLS: u32 = 15;

/// The request text, criteria in the order given; a noul sends no questions, so the server asks `q`.
pub fn body(question: &str, kind: &DecideKind, situation: Option<&str>, cli: &CliArgs) -> String {
    let mut b = Map::new();
    b.insert("q".into(), json!(question));
    let typed = match kind {
        DecideKind::Noul => None,
        DecideKind::Choice(options) => {
            let criteria: Map<String, Value> =
                options.iter().map(|(n, d)| (n.clone(), json!(d))).collect();
            Some(json!({"type": "choice", "criteria": criteria}))
        }
        DecideKind::Score(levels) => Some(json!({"type": "score", "criteria": levels})),
    };
    if let Some(q) = typed {
        b.insert("questions".into(), json!({ "verdict": q }));
    }
    if let Some(s) = situation {
        b.insert("situation".into(), json!(s));
    }
    b.insert("tags".into(), json!(cli.tags));
    if let Some(c) = &cli.category {
        b.insert("category".into(), json!(c));
    }
    Value::Object(b).to_string()
}

/// Fifteen cells, filled to the nearest cell.
pub fn bar(weight: u8, unicode: bool) -> String {
    let filled = (u32::from(weight.min(100)) * CELLS + 50) / 100;
    let (on, off) = if unicode { ('█', '░') } else { ('#', '.') };
    (0..CELLS)
        .map(|i| if i < filled { on } else { off })
        .collect()
}

fn weight(p: f64) -> u8 {
    (p * 100.0).round().clamp(0.0, 100.0) as u8
}

/// Each answer's label and probability in its own order; a score's levels are what `run` adds as `levels`.
fn labelled(answer: &Value, levels: &[String]) -> Vec<(String, f64)> {
    let ps = &answer["probabilities"];
    match answer["type"].as_str() {
        Some("noul") => {
            let yes = answer["noul"].as_f64().unwrap_or(0.0);
            vec![("yes".into(), yes), ("no".into(), 1.0 - yes)]
        }
        Some("choice") => ps
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0)))
                    .collect()
            })
            .unwrap_or_default(),
        _ => ps
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let name = levels
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| format!("level {}", i + 1));
                        (name, v.as_f64().unwrap_or(0.0))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// What an answer picks, as a label: a choice says so, a noul and a score by their heaviest answer.
fn pick(answer: &Value, levels: &[String]) -> Option<String> {
    if let Some(c) = answer["choice"].as_str() {
        return Some(c.to_string());
    }
    labelled(answer, levels)
        .into_iter()
        .fold(None, |b: Option<(String, f64)>, (l, p)| match b {
            Some((_, bp)) if bp >= p => b,
            _ => Some((l, p)),
        })
        .map(|(l, _)| l)
}

fn standing_copy(d: &Value) -> &'static str {
    serde_json::from_value(d["standing"]["verdict"].clone())
        .map(crate::web::decide::standing_copy)
        .unwrap_or("")
}

pub fn render(decision: &Value, unicode: bool) -> String {
    let verdict = &decision["answers"]["verdict"];
    if verdict.is_null() {
        return "Nothing here: the base does not hold this.\n".into();
    }
    let levels: Vec<String> = decision["levels"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let rows = labelled(verdict, &levels);
    let w = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (label, p) in &rows {
        let n = weight(*p);
        out.push_str(&format!("{label:<w$}  {} {n:>3}\n", bar(n, unicode)));
    }
    out.push_str(standing_copy(decision));
    out.push('\n');

    let evidence = decision["evidence"].as_array().cloned().unwrap_or_default();
    if !evidence.is_empty() {
        let chose = pick(verdict, &levels);
        let (tee, last) = if unicode {
            ("├─", "└─")
        } else {
            ("|-", "`-")
        };
        let room = evidence.len().to_string().len();
        out.push_str("\nfrom:\n");
        for (i, e) in evidence.iter().enumerate() {
            let branch = if i + 1 == evidence.len() { last } else { tee };
            let title = match e["title"].as_str().filter(|t| !t.trim().is_empty()) {
                Some(t) => t.replace(['\r', '\n'], " "),
                None => crate::web::markdown::stand_in_title(e["text"].as_str().unwrap_or(""), 60),
            };
            let own = match pick(&e["read"]["verdict"], &levels) {
                Some(p) if Some(&p) != chose.as_ref() => format!("  {p} (against)"),
                Some(p) => format!("  {p}"),
                None => String::new(),
            };
            out.push_str(&format!(
                "{branch} [{:>room$}] {title}{own}  {}\n",
                e["n"].as_u64().unwrap_or(i as u64 + 1),
                e["artifact_id"].as_str().unwrap_or(""),
            ));
        }
    }
    let read = decision["read"].as_u64().unwrap_or(0) as usize;
    let rest = read.saturating_sub(evidence.len());
    if rest > 0 {
        out.push_str(&format!("{rest} more did not bear on it\n"));
    }
    out
}

/// In evidence order, so `--show 2` reaches `[2]`; a row without an id keeps its place.
pub fn evidence_ids(decision: &Value) -> Vec<String> {
    decision["evidence"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| e["artifact_id"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

async fn post(e: &Endpoint, path: &str, body: String) -> Result<reqwest::Response> {
    let res = client()?
        .post(format!("{}?door=cli", e.api(path)))
        .bearer_auth(&e.token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|err| e.unreachable(err))?;
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let said: Value = res.json().await.unwrap_or(Value::Null);
    let error = said["error"].as_str().unwrap_or("no reason given");
    Err(Error::Validation(format!(
        "the question was refused: {status}: {error}"
    )))
}

/// The server's body as it came, and the exit code it means.
async fn json(e: &Endpoint, body: String) -> Result<(String, i32)> {
    let text = post(e, "/decide", body)
        .await?
        .text()
        .await
        .map_err(|err| Error::Validation(format!("{err}")))?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((text, if v["answers"].is_null() { 1 } else { 0 }))
}

pub async fn run(
    e: &Endpoint,
    question: &str,
    kind: &DecideKind,
    situation: Option<&str>,
    cli: &CliArgs,
) -> Result<i32> {
    use tokio_stream::StreamExt;
    let body = body(question, kind, situation, cli);
    if cli.json {
        let (text, code) = json(e, body).await?;
        println!("{text}");
        return Ok(code);
    }
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let face = crate::cli::face::Face::decide(
        cli,
        is_tty,
        std::env::var_os("NO_COLOR").is_some(),
        crate::cli::face::locale().as_deref(),
    );
    let res = post(e, "/decide/stream", body).await?;
    let mut stream = res.bytes_stream();
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = String::new();
    let mut note = crate::cli::ask::Note::default();
    let mut seen = 0;
    let reading = |n: u64, of: u64| {
        face.on.then(|| {
            crate::cli::ask::within(
                &face,
                crate::cli::face::Lamp::Running,
                &format!("reading {n} of {of}"),
            )
        })
    };
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| Error::Validation(format!("{err}")))?;
        pending.extend_from_slice(&chunk);
        buf.push_str(&crate::cli::ask::decode(&mut pending)?);
        for (name, mut data) in crate::cli::ask::frames(&mut buf) {
            match name.as_str() {
                "retrieved" => note.show(reading(0, data["passages"].as_u64().unwrap_or(0))),
                // Reads finish out of rank order, so the count is of frames seen.
                "read" => {
                    seen += 1;
                    note.show(reading(seen, data["of"].as_u64().unwrap_or(0)));
                }
                "done" => {
                    note.clear();
                    // The response names no levels; the client asked them.
                    if let DecideKind::Score(levels) = kind {
                        data["levels"] = json!(levels);
                    }
                    print!("{}", render(&data, face.unicode && face.on));
                    let ids = evidence_ids(&data);
                    if !ids.is_empty() && crate::cli::last::worth_remembering(is_tty, false) {
                        crate::cli::last::save(question, ids);
                    }
                    return Ok(if data["answers"].is_null() { 1 } else { 0 });
                }
                "error" => {
                    note.clear();
                    eprintln!(
                        "{}",
                        data["error"].as_str().unwrap_or("the decision failed")
                    );
                    return Ok(2);
                }
                _ => {}
            }
        }
    }
    Err(Error::Validation(
        "the decision stream ended before a decision".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{CliArgs, DecideKind, Verb};

    #[test]
    fn a_bar_is_fifteen_cells() {
        assert_eq!(bar(81, true), "████████████░░░");
        assert_eq!(bar(19, true), "███░░░░░░░░░░░░");
        assert_eq!(bar(0, true).chars().filter(|c| *c == '█').count(), 0);
        assert_eq!(bar(0, true).chars().count(), 15);
        assert_eq!(bar(100, false), "###############");
    }

    #[test]
    fn the_body_keeps_the_options_in_the_order_given() {
        let cli = CliArgs {
            tags: vec!["infra".into()],
            category: Some("ops".into()),
            ..Default::default()
        };
        let kind = DecideKind::Choice(vec![
            ("switch".into(), "Move".into()),
            ("keep".into(), String::new()),
        ]);
        let text = body("Which?", &kind, Some("12 GB"), &cli);
        assert!(
            text.contains("\"criteria\":{\"switch\":\"Move\",\"keep\":\"\"}"),
            "{text}"
        );
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["q"], "Which?");
        assert_eq!(v["questions"]["verdict"]["type"], "choice");
        assert_eq!(v["situation"], "12 GB");
        assert_eq!(v["tags"], serde_json::json!(["infra"]));
        assert_eq!(v["category"], "ops");

        let v: serde_json::Value =
            serde_json::from_str(&body("Keep?", &DecideKind::Noul, None, &Default::default()))
                .unwrap();
        assert!(v.get("questions").is_none(), "{v}");
        assert!(v.get("situation").is_none(), "{v}");

        let v: serde_json::Value = serde_json::from_str(&body(
            "How soon?",
            &DecideKind::Score(vec!["later".into(), "now".into()]),
            None,
            &Default::default(),
        ))
        .unwrap();
        assert_eq!(
            v["questions"]["verdict"],
            serde_json::json!({"type": "score", "criteria": ["later", "now"]})
        );
    }

    /// The spec's example as the server sends it: two of four passages bear, the second against.
    fn held() -> serde_json::Value {
        let read = |pick: &str, keep: f64| {
            serde_json::json!({"verdict": {"type": "choice", "choice": pick,
                "probabilities": {"keep": keep, "switch to gemma": 1.0 - keep}, "confidence": 0.8}})
        };
        serde_json::json!({
            "answers": {"verdict": {"type": "choice", "choice": "keep",
                "probabilities": {"keep": 0.81, "switch to gemma": 0.19},
                "confidence": 0.62, "swapped_agree": true}},
            "prior": {"verdict": {"type": "choice", "choice": "keep",
                "probabilities": {"keep": 0.55, "switch to gemma": 0.45}, "confidence": 0.07}},
            "standing": {"verdict": "settled"},
            "evidence": [
                {"n": 1, "artifact_id": "01AAA", "title": "Reindex notes 09/14", "text": "…",
                 "bears": 0.93, "joint": true, "read": read("keep", 0.9)},
                {"n": 2, "artifact_id": "01BBB", "title": "GPU budget", "text": "…",
                 "bears": 0.8, "joint": true, "read": read("switch to gemma", 0.2)},
            ],
            "read": 4,
            "dropped": 0,
            "model": "rule"
        })
    }

    #[test]
    fn a_decision_prints_bars_a_standing_and_its_evidence() {
        assert_eq!(
            render(&held(), true),
            "keep             ████████████░░░  81\n\
             switch to gemma  ███░░░░░░░░░░░░  19\n\
             Settled.\n\
             \n\
             from:\n\
             ├─ [1] Reindex notes 09/14  keep  01AAA\n\
             └─ [2] GPU budget  switch to gemma (against)  01BBB\n\
             2 more did not bear on it\n"
        );
    }

    #[test]
    fn yes_or_no_and_a_scale_name_their_answers() {
        let noul = serde_json::json!({
            "answers": {"verdict": {"type": "noul", "noul": 0.7}},
            "prior": {"verdict": {"type": "noul", "noul": 0.5}},
            "standing": {"verdict": "leaning"},
            "evidence": [], "read": 0, "dropped": 0, "model": "rule"
        });
        let out = render(&noul, false);
        assert!(
            out.starts_with("yes  ###########....  70\nno   #####..........  30\nLeaning.\n"),
            "{out}"
        );

        let mut scale = serde_json::json!({
            "answers": {"verdict": {"type": "score", "score": 1.1,
                "probabilities": [0.1, 0.7, 0.2], "confidence": 0.5}},
            "prior": {"verdict": {"type": "score", "score": 1.0,
                "probabilities": [0.3, 0.4, 0.3], "confidence": 0.1}},
            "standing": {"verdict": "split"},
            "evidence": [], "read": 0, "dropped": 0, "model": "rule"
        });
        scale["levels"] = serde_json::json!(["can wait", "this month", "now"]);
        let out = render(&scale, true);
        assert!(out.contains("this month  ███████████░░░░  70\n"), "{out}");
        assert!(out.contains("Split."), "{out}");
    }

    #[test]
    fn a_decision_the_base_does_not_hold_prints_the_notice_alone() {
        let mut d = held();
        d["answers"] = serde_json::Value::Null;
        d["standing"] = serde_json::json!({"verdict": "not_held"});
        d["evidence"] = serde_json::json!([]);
        assert_eq!(
            render(&d, true),
            "Nothing here: the base does not hold this.\n"
        );
    }

    #[test]
    fn no_percent_sign_and_no_likelihood() {
        let out = render(&held(), true);
        assert!(
            !out.contains('%') && !out.to_lowercase().contains("likely"),
            "{out}"
        );
    }

    #[test]
    fn the_evidence_ids_keep_their_ranks() {
        let mut d = held();
        d["evidence"][0]
            .as_object_mut()
            .unwrap()
            .remove("artifact_id");
        assert_eq!(evidence_ids(&d), vec![String::new(), "01BBB".to_string()]);
    }

    async fn seeded() -> crate::core::Core {
        crate::core::decide::tests::three_base().await
    }

    #[tokio::test]
    async fn a_decision_streams_and_the_stream_terminates() {
        let (url, token) = crate::cli::test_support::serve(seeded().await).await;
        let e = Endpoint { url, token };
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run(
                &e,
                "Keep bge-m3?",
                &DecideKind::Noul,
                None,
                &Default::default(),
            ),
        )
        .await
        .expect("the stream must end")
        .expect("the stream must not fail");
        assert!(code == 0 || code == 1, "unexpected exit code {code}");
    }

    #[tokio::test]
    async fn nothing_held_exits_one() {
        let (url, token, _core) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        let code = run(
            &e,
            "Keep bge-m3?",
            &DecideKind::Noul,
            None,
            &Default::default(),
        )
        .await
        .unwrap();
        assert_eq!(code, 1);
    }

    #[tokio::test]
    async fn json_prints_the_servers_body() {
        let (url, token) = crate::cli::test_support::serve(seeded().await).await;
        let e = Endpoint { url, token };
        let (printed, code) = json(
            &e,
            body("Keep bge-m3?", &DecideKind::Noul, None, &Default::default()),
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&printed).unwrap();
        assert!(v.get("standing").is_some(), "{printed}");
        assert_eq!(code, if v["answers"].is_null() { 1 } else { 0 });
    }

    #[tokio::test]
    async fn a_refused_question_exits_two_through_run() {
        let (url, token, _core) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        let verb = Verb::Decide {
            question: String::new(),
            kind: DecideKind::Noul,
            situation: None,
        };
        assert_eq!(crate::cli::dispatch(&e, verb, &Default::default()).await, 2);
        let verb = Verb::Decide {
            question: String::new(),
            kind: DecideKind::Noul,
            situation: None,
        };
        let json = CliArgs {
            json: true,
            ..Default::default()
        };
        assert_eq!(crate::cli::dispatch(&e, verb, &json).await, 2);
    }

    #[tokio::test]
    async fn an_unreachable_server_exits_two() {
        let e = Endpoint {
            url: "http://127.0.0.1:9".into(),
            token: "engram_x".into(),
        };
        let verb = Verb::Decide {
            question: "Keep it?".into(),
            kind: DecideKind::Noul,
            situation: None,
        };
        assert_eq!(crate::cli::dispatch(&e, verb, &Default::default()).await, 2);
    }

    #[tokio::test]
    async fn the_client_claims_the_cli_door() {
        let mut core = seeded().await;
        core.learn.enabled = true;
        let pool = core.store.pool.clone();
        let (url, token) = crate::cli::test_support::serve(core).await;
        let e = Endpoint { url, token };
        run(
            &e,
            "Keep bge-m3?",
            &DecideKind::Noul,
            None,
            &Default::default(),
        )
        .await
        .unwrap();
        let n: i64 = sqlx::query_scalar!("SELECT count(*) FROM decide_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "a CLI decision is recorded under the cli door");
    }
}
