#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Decide driver in headless Chrome (`--ignored`: needs `node` and Chrome). A left-open
//! `EventSource` reconnects and reads every passage again, so the harness counts streams.

use std::process::Command;

#[path = "browser/chrome.rs"]
mod chrome;
use chrome::chrome;

fn drive(scenario: &str) -> serde_json::Value {
    let root = env!("CARGO_MANIFEST_DIR");
    let chrome = chrome().expect(
        "no headless Chrome found. Set ENGRAM_CHROME to one, or install Playwright's \
         chrome-headless-shell, to run this test.",
    );
    let out = Command::new("node")
        .arg(format!("{root}/tests/browser/decide_stream.js"))
        .arg(root)
        .arg(&chrome)
        .arg(scenario)
        .output()
        .expect("node is needed to run this test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .last()
        .unwrap_or_else(|| panic!("the harness printed nothing: {stdout}"));
    let v: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
    assert!(!v["report"].is_null(), "the page never reported back: {v}");
    v
}

fn no_errors(report: &serde_json::Value) {
    assert_eq!(report["errors"].as_array().unwrap().len(), 0, "{report}");
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn one_decision_opens_one_stream_and_leaves_none_open() {
    let out = drive("single");
    let r = &out["report"];
    assert_eq!(out["parkRequests"], 1, "{out}");
    assert_eq!(out["streamRequests"], 1, "the stream reconnected: {out}");
    assert_eq!(out["openStreams"], 0, "the stream was left open: {out}");
    assert_eq!(r["rowOpenAfterFirstPress"], true, "{r}");
    assert_eq!(
        out["bodies"][0]["body"], r#"{"q":"Should I keep it?"}"#,
        "{out}"
    );
    assert_eq!(out["bodies"][0]["type"], "application/json", "{out}");
    let seen: Vec<&str> = r["progressSeen"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(seen.contains(&"Reading 1 of 3"), "{seen:?}");
    assert!(seen.contains(&"Weighing 2 together"), "{seen:?}");
    assert!(
        r["resultHtml"].as_str().unwrap().contains("bar-track"),
        "{r}"
    );
    assert_eq!(r["railRows"], 2, "{r}");
    assert!(
        r["railHead"]
            .as_str()
            .unwrap()
            .starts_with("Decided from 2 of 3"),
        "{r}"
    );
    assert_eq!(r["rerun"], true, "{r}");
    assert_eq!(r["status"], "The decision is ready.", "{r}");
    assert_eq!(r["formBusy"], false, "{r}");
    no_errors(r);
}
