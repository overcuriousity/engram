#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The ask stream driver in headless Chrome (`--ignored`: needs `node` and Chrome). An `EventSource`
//! left open reconnects and re-asks, a second model call, so the harness counts stream requests.

use std::path::PathBuf;
use std::process::Command;

fn chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ENGRAM_CHROME") {
        return Some(PathBuf::from(p));
    }
    if let Ok(home) = std::env::var("HOME") {
        let cache = PathBuf::from(&home).join(".cache/ms-playwright");
        if let Ok(entries) = std::fs::read_dir(&cache) {
            let mut found: Vec<(u64, PathBuf)> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let bin = e
                        .path()
                        .join("chrome-headless-shell-linux64/chrome-headless-shell");
                    // Playwright's build number from the directory suffix (`chromium_headless_shell-1148`), else zero.
                    let build = e
                        .file_name()
                        .to_string_lossy()
                        .rsplit('-')
                        .next()
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                    bin.exists().then_some((build, bin))
                })
                .collect();
            // Newest build wins, by number: sorting paths puts `-999` after `-1148`.
            found.sort();
            if let Some((_, p)) = found.pop() {
                return Some(p);
            }
        }
    }
    for name in ["chrome-headless-shell", "chromium", "google-chrome"] {
        if let Ok(out) = Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {name}"))
            .output()
            && out.status.success()
        {
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    None
}

fn drive(scenario: &str) -> serde_json::Value {
    let root = env!("CARGO_MANIFEST_DIR");
    let chrome = chrome().expect(
        "no headless Chrome found. Set ENGRAM_CHROME to one, or install Playwright's \
         chrome-headless-shell, to run this test.",
    );
    let out = Command::new("node")
        .arg(format!("{root}/tests/browser/ask_stream.js"))
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
    serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"))
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn one_ask_opens_one_stream_and_leaves_none_open_behind_it() {
    let out = drive("single");
    let report = &out["report"];
    assert!(!report.is_null(), "the page never reported back: {out}");

    assert_eq!(out["parkRequests"], 1, "{out}");
    assert_eq!(
        out["streamRequests"], 1,
        "the browser reconnected, which means an EventSource was left open — \
         every reconnect is a model call nobody asked for: {out}"
    );

    let result = report["result"].as_str().unwrap();
    assert!(
        result.contains(r##"href="#cite-1""##),
        "the done fragment was not swapped in: {result}"
    );
    assert!(
        !result.contains("lost"),
        "the finished answer was overwritten by a connection error: {result}"
    );
    assert_eq!(report["liveHidden"], true, "{report}");
    assert_eq!(report["reasoningHidden"], true, "{report}");
    assert!(
        report["liveText"]
            .as_str()
            .unwrap()
            .contains("alpha [1] and"),
        "the tokens did not accumulate: {report}"
    );
    assert!(
        !report["statusText"].as_str().unwrap().is_empty(),
        "nothing announced the finished answer: {report}"
    );
    // The planned queries survive round two's statistics landing after them.
    assert_eq!(
        report["progressText"],
        "Looking further: more about bravo, charlie \u{2014} searched 3, showing 2",
        "{report}"
    );
    assert_eq!(report["railIds"][1], "cite-2", "{report}");
    assert_eq!(report["activeId"], "cite-2", "{report}");
    assert_eq!(report["formAsking"], false, "{report}");
    // `innerHTML` is unwatched by htmx; the buttons work only because the driver hands the fragment over.
    assert_eq!(
        out["verdictRequests"], 1,
        "the verdict button in the streamed answer was inert: {out}"
    );
    assert!(
        result.contains("judged"),
        "htmx did not swap the verdict response: {result}"
    );
    assert_eq!(report["errors"].as_array().unwrap().len(), 0, "{report}");
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn a_second_ask_supersedes_the_first_rather_than_racing_it() {
    let out = drive("double");
    let report = &out["report"];
    assert!(!report.is_null(), "the page never reported back: {out}");

    assert_eq!(
        out["parkRequests"], 2,
        "both submits must reach the door: {out}"
    );
    assert_eq!(
        out["streamRequests"], 1,
        "the superseded ask opened a stream of its own: {out}"
    );

    let result = report["result"].as_str().unwrap();
    assert!(
        result.contains(r##"href="#cite-1""##) && !result.contains("lost"),
        "the surviving ask did not get to finish: {result}"
    );
    assert_eq!(report["errors"].as_array().unwrap().len(), 0, "{report}");
}
