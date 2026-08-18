//! The ask page's stream driver, in a real browser.
//!
//! Ignored by default: this needs `node` and a headless Chrome, neither of
//! which `cargo test` may assume. Run it with
//!
//! ```text
//! cargo test --test browser_ask -- --ignored
//! ```
//!
//! It exists because the most consequential line in `assets/app.js` —
//! `source.close()` — guards a behaviour that only a browser performs. An
//! `EventSource` left open reconnects on its own when the stream ends, and the
//! reconnect asks the question again: a model call nobody requested, and on a
//! metered endpoint a second bill. Nothing in the Rust suite can see that, and
//! the browser reports it as success. So the harness counts stream requests
//! against a fake server and these tests assert the count.
//!
//! `web::ui::tests::the_stream_driver_closes_the_event_source_on_every_exit`
//! is the cheap always-run half of this: it fails if the calls are deleted from
//! the source. This is the half that proves they do what they are there for.

use std::path::PathBuf;
use std::process::Command;

/// The headless browser to drive, or `None` when the machine has none.
///
/// `ENGRAM_CHROME` first, so a machine with a browser in an unusual place can
/// say where. Playwright's download is checked next because a developer working
/// on this repo's front end is likely to have it already.
fn chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ENGRAM_CHROME") {
        return Some(PathBuf::from(p));
    }
    if let Ok(home) = std::env::var("HOME") {
        let cache = PathBuf::from(&home).join(".cache/ms-playwright");
        if let Ok(entries) = std::fs::read_dir(&cache) {
            let mut found: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| {
                    e.path()
                        .join("chrome-headless-shell-linux64/chrome-headless-shell")
                })
                .filter(|p| p.exists())
                .collect();
            // Newest install wins, so an old download is not preferred forever.
            found.sort();
            if let Some(p) = found.pop() {
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

/// One run of the harness, as JSON.
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

/// The stream is opened once and closed when the answer is done.
///
/// The count is the assertion. A driver that forgets to close would still show
/// the right answer on the page — and then quietly ask the question a second
/// time about three seconds later, which is why the harness waits six.
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
    // The two transient regions gave way to the rendered answer, and the
    // finished answer announced itself once.
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
    // The rail arrived with its ids, and a citation click marked its excerpt.
    assert_eq!(report["railIds"][1], "cite-2", "{report}");
    assert_eq!(report["activeId"], "cite-2", "{report}");
    assert_eq!(report["formAsking"], false, "{report}");
    assert_eq!(report["errors"].as_array().unwrap().len(), 0, "{report}");
}

/// A double-tap on Ask leaves exactly one stream, and the answer survives it.
///
/// Two submits made before the first POST resolves used to open two streams,
/// the second overwriting the only reference to the first. The orphan could not
/// be closed, and its error — which arrives the moment its stream ends — ran
/// the failure path, wiping the answer the second ask was still writing.
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
