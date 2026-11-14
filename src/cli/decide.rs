//! `-d`: one typed question, decided from the notes, printed as weights and what they rest on.

use crate::cli::args::{CliArgs, DecideKind};
use crate::cli::endpoint::{Endpoint, client};
use crate::core::decide::wire::answer_from;
use crate::error::{Error, Result};
use crate::infer::decide::{Kind, Question};
use crate::web::decide::{pick_label, weights};
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

/// The question as the server asks it, so its answers read back with the server's own parser.
pub fn question(question: &str, kind: &DecideKind) -> Question {
    let kind = match kind {
        DecideKind::Noul => Kind::Noul,
        DecideKind::Choice(options) => Kind::Choice {
            options: options.clone(),
        },
        DecideKind::Score(levels) => Kind::Score {
            levels: levels.clone(),
        },
    };
    Question {
        key: "verdict".into(),
        kind,
        instructions: question.into(),
    }
}

fn standing_copy(d: &Value) -> &'static str {
    serde_json::from_value(d["standing"]["verdict"].clone())
        .map(crate::web::decide::standing_copy)
        .unwrap_or("")
}

pub fn render(decision: &Value, q: &Question, unicode: bool) -> String {
    let Some(verdict) = answer_from(q, &decision["answers"][&q.key]) else {
        return "Nothing here: the base does not hold this.\n".into();
    };
    let rows = weights(q, &verdict);
    let w = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (label, n) in &rows {
        let n = *n;
        out.push_str(&format!("{label:<w$}  {} {n:>3}\n", bar(n, unicode)));
    }
    out.push_str(standing_copy(decision));
    out.push('\n');

    let evidence = decision["evidence"].as_array().cloned().unwrap_or_default();
    if !evidence.is_empty() {
        let chose = pick_label(q, &verdict);
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
            let own = match answer_from(q, &e["read"][&q.key]).map(|a| pick_label(q, &a)) {
                Some(p) if p != chose => format!("  {p} (against)"),
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
    let v: Value = serde_json::from_str(&text)
        .map_err(|err| Error::Validation(format!("the decision did not arrive as JSON: {err}")))?;
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
    let q = self::question(question, kind);
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
        for (name, data) in crate::cli::ask::frames(&mut buf) {
            match name.as_str() {
                "retrieved" => note.show(reading(0, data["passages"].as_u64().unwrap_or(0))),
                // Reads finish out of rank order, so the count is of frames seen.
                "read" => {
                    seen += 1;
                    note.show(reading(seen, data["of"].as_u64().unwrap_or(0)));
                }
                "done" => {
                    note.clear();
                    print!("{}", render(&data, &q, face.unicode && face.on));
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

    fn held_q() -> Question {
        let options = ["switch to gemma", "keep"];
        question(
            "Keep?",
            &DecideKind::Choice(
                options
                    .iter()
                    .map(|n| (n.to_string(), String::new()))
                    .collect(),
            ),
        )
    }

    /// The spec's example as the server sends it, options not in alphabetical order: two of four passages bear, the second against.
    fn held() -> serde_json::Value {
        let read = |pick: &str, keep: f64| {
            serde_json::json!({"verdict": {"type": "choice", "choice": pick,
                "probabilities": {"switch to gemma": 1.0 - keep, "keep": keep}, "confidence": 0.8}})
        };
        serde_json::json!({
            "answers": {"verdict": {"type": "choice", "choice": "keep",
                "probabilities": {"switch to gemma": 0.19, "keep": 0.81},
                "confidence": 0.62, "swapped_agree": true}},
            "prior": {"verdict": {"type": "choice", "choice": "keep",
                "probabilities": {"switch to gemma": 0.45, "keep": 0.55}, "confidence": 0.07}},
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
            render(&held(), &held_q(), true),
            "switch to gemma  ███░░░░░░░░░░░░  19\n\
             keep             ████████████░░░  81\n\
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
        let out = render(&noul, &question("Keep?", &DecideKind::Noul), false);
        assert!(
            out.starts_with("yes  ###########....  70\nno   #####..........  30\nLeaning.\n"),
            "{out}"
        );

        let scale = serde_json::json!({
            "answers": {"verdict": {"type": "score", "score": 1.1,
                "probabilities": [0.1, 0.7, 0.2], "confidence": 0.5}},
            "prior": {"verdict": {"type": "score", "score": 1.0,
                "probabilities": [0.3, 0.4, 0.3], "confidence": 0.1}},
            "standing": {"verdict": "split"},
            "evidence": [], "read": 0, "dropped": 0, "model": "rule"
        });
        let levels = DecideKind::Score(vec!["can wait".into(), "this month".into(), "now".into()]);
        let out = render(&scale, &question("How soon?", &levels), true);
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
            render(&d, &held_q(), true),
            "Nothing here: the base does not hold this.\n"
        );
    }

    #[test]
    fn no_percent_sign_and_no_likelihood() {
        let out = render(&held(), &held_q(), true);
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
        assert_eq!(code, 0, "the fake decider holds a seeded base");
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
        assert_eq!(code, 0, "{printed}");
    }

    #[tokio::test]
    async fn a_reply_that_is_not_json_is_a_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let addr = listener.local_addr().expect("the port it got");
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (mut sock, _) = listener.accept().await.expect("a client");
            // Read the request first: closing it unread sends a reset.
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let reply = "<html>a proxy page</html>";
            sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            )
            .await
            .ok();
            sock.flush().await.ok();
        });
        let e = Endpoint {
            url: format!("http://{addr}"),
            token: "engram_x".into(),
        };
        let verb = Verb::Decide {
            question: "Keep it?".into(),
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

    /// Keep 0.81 on the joint read, 0.9 on each passage's own, so a door that showed a passage's numbers would fail.
    fn keep_on_the_joint_read(
        state: &str,
        qs: &[crate::infer::decide::Question],
    ) -> Result<crate::infer::decide::Answers> {
        use crate::infer::decide::{Answer, Answers, Kind};
        let keep = if state.contains("Excerpt 1") {
            0.81
        } else {
            0.9
        };
        let answers = qs
            .iter()
            .map(|q| {
                let a = match &q.kind {
                    _ if q.key == "bears" => Answer::Noul {
                        yes: if state.contains("zebracrossing") {
                            0.95
                        } else {
                            0.02
                        },
                    },
                    Kind::Choice { options } => Answer::Choice {
                        pick: "keep".into(),
                        probabilities: options
                            .iter()
                            .map(|(n, _)| if n == "keep" { keep } else { 1.0 - keep })
                            .collect(),
                        confidence: keep,
                    },
                    _ => Answer::Noul { yes: keep },
                };
                (q.key.clone(), a)
            })
            .collect();
        Ok(Answers {
            answers,
            model: "rule".into(),
            input_tokens: 0,
        })
    }

    #[tokio::test]
    async fn the_same_question_is_the_same_decision_through_every_door() {
        let mut core = seeded().await;
        crate::core::decide::tests::deciding(&mut core, keep_on_the_joint_read);
        let (url, token) = crate::cli::test_support::serve(core.clone()).await;
        let e = Endpoint { url, token };
        let (q, options) = ("Which embedder?", ["keep", "switch"]);
        let kind = DecideKind::Choice(
            options
                .iter()
                .map(|n| (n.to_string(), String::new()))
                .collect(),
        );
        let weights = |v: &Value| -> Vec<(String, u8)> {
            v["answers"]["verdict"]["probabilities"]
                .as_object()
                .expect("a choice's probabilities by name")
                .iter()
                .map(|(k, p)| (k.clone(), (p.as_f64().unwrap() * 100.0).round() as u8))
                .collect()
        };
        let evidence = |v: &Value| -> Vec<(String, String)> {
            v["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    (
                        e["artifact_id"].as_str().unwrap().to_string(),
                        e["title"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        };
        let want = vec![("keep".to_string(), 81), ("switch".to_string(), 19)];

        let rest: Value = reqwest::Client::new()
            .post(format!("{}/api/v1/decide", e.url))
            .bearer_auth(&e.token)
            .json(
                &serde_json::from_str::<Value>(&body(q, &kind, None, &Default::default())).unwrap(),
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(weights(&rest), want, "REST: {rest}");
        assert_eq!(rest["standing"]["verdict"], "settled", "REST: {rest}");
        let ev = evidence(&rest);
        assert_eq!(ev.len(), 2, "the two marked notes bear: {rest}");

        let (text, code) = json(&e, body(q, &kind, None, &Default::default()))
            .await
            .unwrap();
        let cli: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(code, 0);
        assert_eq!(weights(&cli), want, "CLI: {text}");
        assert_eq!(cli["standing"], rest["standing"], "CLI: {text}");
        assert_eq!(evidence(&cli), ev, "CLI: {text}");
        let printed = render(&cli, &question(q, &kind), true);
        assert!(
            printed.contains(" 81\n") && printed.contains(" 19\n") && printed.contains("Settled."),
            "{printed}"
        );

        use rmcp::handler::server::wrapper::Parameters;
        let mcp = crate::mcp::PkdbTools::over(core.clone())
            .decide(Parameters(crate::mcp::DecideParams {
                q: q.into(),
                options: Some(
                    options
                        .iter()
                        .map(|n| crate::mcp::DecideOption {
                            name: n.to_string(),
                            description: None,
                        })
                        .collect(),
                ),
                levels: None,
                situation: None,
            }))
            .await;
        assert!(
            mcp.contains("- keep: 81\n") && mcp.contains("- switch: 19\n"),
            "{mcp}"
        );
        assert!(mcp.contains("Settled."), "{mcp}");
        let at: Vec<usize> = ev
            .iter()
            .map(|(_, t)| {
                mcp.find(&format!(". {t}"))
                    .unwrap_or_else(|| panic!("{t}: {mcp}"))
            })
            .collect();
        assert!(at.is_sorted(), "MCP keeps the evidence order: {mcp}");

        let req = crate::core::decide::DecideRequest {
            q: q.into(),
            questions: vec![crate::infer::decide::Question {
                key: "verdict".into(),
                kind: crate::infer::decide::Kind::Choice {
                    options: options
                        .iter()
                        .map(|n| (n.to_string(), String::new()))
                        .collect(),
                },
                instructions: String::new(),
            }],
            ..Default::default()
        };
        let d = core
            .decide(&req, crate::store::feedback::Door::Ui)
            .await
            .unwrap();
        let ids: Vec<String> = d.evidence.iter().map(|e| e.artifact_id.clone()).collect();
        assert_eq!(ids, ev.iter().map(|(i, _)| i.clone()).collect::<Vec<_>>());
        let html = crate::web::decide::decision_fragment(&d, None).unwrap();
        let (k, s) = (html.find(r#"width="81""#), html.find(r#"width="19""#));
        assert!(k.is_some() && s.is_some() && k < s, "{html}");
        assert!(html.contains("Settled."), "{html}");
    }
}
