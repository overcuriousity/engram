//! `--status`: what the base holds, what it is working through, and what it has
//! been learning.
//!
//! One shot, and no ambient version of it. A footer under every search would
//! make the cheapest door in the application pay a request for something nobody
//! asked for at that moment.

use crate::cli::endpoint::Endpoint;
use crate::cli::face::{Face, Lamp};
use crate::error::{Error, Result};

/// Ask the server what it is doing, and say it.
pub async fn run(e: &Endpoint, face: &Face, json: bool) -> Result<i32> {
    let http = reqwest::Client::builder()
        .user_agent(crate::cli::capture::USER_AGENT)
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
    let res = http
        .get(e.api("/status"))
        .bearer_auth(&e.token)
        .send()
        .await
        .map_err(|err| Error::Validation(format!("{err}")))?;
    let status = res.status();
    let body = res
        .text()
        .await
        .map_err(|err| Error::Validation(format!("{err}")))?;
    if !status.is_success() {
        return Err(Error::Validation(format!(
            "the status was refused: {status}"
        )));
    }
    if json {
        // The server's own body, unchanged, by the rule `-s --json` follows.
        println!("{body}");
        return Ok(0);
    }
    let said: serde_json::Value =
        serde_json::from_str(&body).map_err(|err| Error::Internal(format!("status: {err}")))?;
    print!("{}", render(face, e, &said));
    // What is due, from a second read that an older server answers 404 to —
    // in which case nothing is said, rather than the status failing.
    if let Ok(res) = http.get(e.api("/moments?kind=due")).bearer_auth(&e.token).send().await
        && res.status().is_success()
        && let Ok(due) = res.json::<serde_json::Value>().await
    {
        print!("{}", render_due(face, &due));
    }
    Ok(0)
}

/// The Due block: one line per open reminder, overdue marked, undated last.
/// Empty when nothing is due, so a quiet base prints nothing extra.
pub(crate) fn render_due(face: &Face, rows: &serde_json::Value) -> String {
    let Some(rows) = rows.as_array().filter(|r| !r.is_empty()) else { return String::new() };
    let now = crate::store::now();
    let mut out = String::from("\n  due\n");
    for r in rows {
        let title = r["title"].as_str().unwrap_or("");
        let line = match r["moment"]["at"].as_i64() {
            None => format!("    (undated)  {title}"),
            Some(at) if at < now => format!("    {}  {title}", face.ink_dim("overdue")),
            Some(at) => format!("    {}  {title}", crate::web::ui::ago_or_ahead(at)),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The screen, as a string. Split from `run` for the reason every renderer in
/// this module is: what is said is worth asserting, and it cannot be asserted
/// through a function whose only output is stdout.
pub(crate) fn render(face: &Face, e: &Endpoint, s: &serde_json::Value) -> String {
    let mut out = String::new();
    let host = e
        .url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let held = format!(
        "{} artifacts · {} vectors",
        s["chunks"].as_i64().unwrap_or(0),
        s["vectors"].as_u64().unwrap_or(0)
    );
    out.push_str(&format!("\n  {host}   {}\n\n", face.ink_dim(&held)));

    // Corpora by status. `needs review` and `failed` are the two a person can
    // act on, so they are the two that carry a colour — and they say their own
    // names either way.
    if let Some(rows) = s["sources"].as_array() {
        out.push_str(&format!("  sources    {}\n", counts(face, rows)));
    }
    if let Some(rows) = s["jobs"].as_array() {
        let mut line = counts(face, rows);
        if let Some(age) = s["oldest_pending_secs"].as_i64() {
            line.push_str(&format!(
                " · {}",
                face.ink_dim(&format!("oldest {}", ago(age)))
            ));
        }
        out.push_str(&format!("  jobs       {line}\n"));
    }

    // Absent while `[learn]` is off, which is the honest answer: four zeroes
    // read like a faculty failing, and one that was never switched on is not.
    if let Some(l) = s["learning"].as_object() {
        let n = |k: &str| l.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
        out.push_str(&format!(
            "\n  learning   {} pursuits open · {} closed unsatisfied · {} gaps\n",
            n("pursuits_open"),
            n("pursuits_unsatisfied"),
            n("gaps_open"),
        ));
        out.push_str(&format!(
            "             {} artifacts written from pursuits\n",
            n("from_pursuits")
        ));
    }

    // What the last reap sweep did, and how many retired rows still hold
    // their text. Absent from an older server, absent while the sweep is off
    // — either way the key is missing and the line with it.
    if let Some(r) = s["reap"].as_object() {
        let n = |k: &str| r.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
        out.push_str(&format!(
            "\n  reap       {} judged · {} reaped · {} rescued · {} retired waiting\n",
            n("judged"),
            n("reaped"),
            n("rescued"),
            n("retired_waiting"),
        ));
    }

    // The rows, not only the count: a failed job nobody can name is a failed
    // job nobody can retry.
    if let Some(failed) = s["failed"].as_array().filter(|f| !f.is_empty()) {
        out.push_str(&format!(
            "\n  {}\n",
            face.lamp_line(Lamp::Stopped, &format!("{} failed jobs", failed.len()))
        ));
        for f in failed.iter().take(10) {
            let stage = f["stage"].as_str().unwrap_or("?");
            let subject: String = f["target_id"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(8)
                .collect();
            let why = f["last_error"].as_str().unwrap_or("no reason recorded");
            out.push_str(&format!("     {stage:<8} {subject:<10} {why}\n"));
        }
    }
    out
}

/// `1 780 ready   3 embedding   2 failed`, with the two anyone can act on lit.
fn counts(face: &Face, rows: &[serde_json::Value]) -> String {
    rows.iter()
        .filter_map(|r| {
            let name = r.get(0)?.as_str()?;
            let n = r.get(1)?.as_i64()?;
            let said = format!("{n} {}", name.replace('_', " "));
            Some(match name {
                "failed" => face.ink_bad(&said),
                "needs_review" => face.ink_caution(&said),
                _ => said,
            })
        })
        .collect::<Vec<_>>()
        .join("   ")
}

/// A duration a person reads rather than a count of seconds.
fn ago(secs: i64) -> String {
    match secs {
        s if s < 90 => format!("{s} s"),
        s if s < 5_400 => format!("{} min", s / 60),
        s if s < 172_800 => format!("{} h", s / 3_600),
        s => format!("{} d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> Endpoint {
        Endpoint {
            url: "https://engram.mikoshi.de".into(),
            token: "engram_x".into(),
        }
    }

    fn face() -> Face {
        Face::decide(
            &crate::cli::args::CliArgs {
                fancy: crate::cli::args::Fancy::Always,
                ..Default::default()
            },
            true,
            false,
            Some("en_US.UTF-8"),
        )
    }

    fn body(learning: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "sources": [["ready", 1780], ["embedding", 3], ["needs_review", 1]],
            "jobs": [["pending", 4], ["running", 1]],
            "failed": [],
            "oldest_pending_secs": 38,
            "chunks": 1842,
            "vectors": 1842,
            "learning": learning,
        })
    }

    /// A faculty that was never switched on is not a faculty at zero.
    #[test]
    fn nothing_is_said_about_learning_while_the_layer_is_off() {
        let out = render(&face(), &endpoint(), &body(serde_json::Value::Null));
        assert!(!out.contains("learning"), "{out}");
        assert!(out.contains("1842 artifacts"), "{out}");
        assert!(out.contains("engram.mikoshi.de"), "{out}");
    }

    /// The reap line: what the last sweep did, and how many retired rows
    /// still wait with their text. Absent from an older server's JSON, so
    /// absent from the render too.
    #[test]
    fn the_reap_line_says_what_was_done_and_what_waits() {
        let mut b = body(serde_json::Value::Null);
        b["reap"] = serde_json::json!({
            "judged": 3, "reaped": 2, "rescued": 1, "retired_waiting": 41
        });
        let out = render(&face(), &endpoint(), &b);
        assert!(
            out.contains("3 judged · 2 reaped · 1 rescued · 41 retired waiting"),
            "{out}"
        );
        let without = render(&face(), &endpoint(), &body(serde_json::Value::Null));
        assert!(!without.contains("reap"), "{without}");
    }

    /// The half that was invisible from a shell: a pursuit closed unsatisfied
    /// is a hole somebody went looking through and did not fill.
    #[test]
    fn the_learning_half_is_counted_and_named() {
        let out = render(
            &face(),
            &endpoint(),
            &body(serde_json::json!({
                "pursuits_open": 7,
                "pursuits_unsatisfied": 4,
                "from_pursuits": 3,
                "gaps_open": 11
            })),
        );
        assert!(out.contains("7 pursuits open"), "{out}");
        assert!(out.contains("4 closed unsatisfied"), "{out}");
        assert!(out.contains("11 gaps"), "{out}");
        assert!(out.contains("3 artifacts written from pursuits"), "{out}");
    }

    /// Serialised from the struct the server actually sends, not from a
    /// hand-written object: the keys drifted once already, and a fixture that
    /// spells them itself cannot notice.
    fn failed(target_id: &str, last_error: Option<&str>) -> serde_json::Value {
        serde_json::to_value(crate::store::jobs::FailedJob {
            id: 1,
            stage: "embed".into(),
            target_id: target_id.into(),
            attempts: 5,
            last_error: last_error.map(str::to_string),
        })
        .expect("a failed job serialises")
    }

    /// A failed job nobody can name is a failed job nobody can retry.
    #[test]
    fn a_failed_job_is_named_and_not_only_counted() {
        let mut b = body(serde_json::Value::Null);
        b["failed"] = serde_json::json!([failed("01J8Z4K2QW7NR3T9", Some("connection refused"))]);
        let out = render(&face(), &endpoint(), &b);
        assert!(out.contains("1 failed jobs"), "{out}");
        assert!(out.contains("01J8Z4K2"), "{out}");
        assert!(out.contains("connection refused"), "{out}");
    }

    /// Everything colour says here is said in words as well.
    #[test]
    fn the_screen_survives_having_every_escape_stripped() {
        let bare = Face {
            color: false,
            ..face()
        };
        let mut b = body(serde_json::json!({
            "pursuits_open": 7, "pursuits_unsatisfied": 4, "from_pursuits": 3, "gaps_open": 11
        }));
        b["failed"] = serde_json::json!([failed("01J8Z4K2", Some("connection refused"))]);
        let stripped = crate::cli::face::strip_sgr(&render(&face(), &endpoint(), &b));
        assert_eq!(stripped, render(&bare, &endpoint(), &b));
    }

    #[test]
    fn an_age_is_read_rather_than_counted_in_seconds() {
        assert_eq!(ago(38), "38 s");
        assert_eq!(ago(600), "10 min");
        assert_eq!(ago(14_400), "4 h");
        assert_eq!(ago(200_000), "2 d");
    }

    #[test]
    fn what_is_due_is_listed_after_the_status_overdue_marked_undated_last() {
        let now = crate::store::now();
        let rows = serde_json::json!([
            { "title": "Send the invoice", "moment": { "at": now - 3_600 } },
            { "title": "Call the bank", "moment": { "at": now + 7_200 } },
            { "title": "Something, sometime", "moment": { "at": null } },
        ]);
        let out = render_due(&face(), &rows);
        let (a, b, c) = (
            out.find("Send the invoice").unwrap(),
            out.find("Call the bank").unwrap(),
            out.find("Something, sometime").unwrap(),
        );
        assert!(a < b && b < c, "{out}");
        assert!(out.contains("overdue"), "{out}");
        assert!(out.contains("in 2 h"), "{out}");
        assert!(out.contains("(undated)"), "{out}");
        assert_eq!(render_due(&face(), &serde_json::json!([])), "", "a quiet base says nothing");
    }
}
