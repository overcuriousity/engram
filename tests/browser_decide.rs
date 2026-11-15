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
    assert_eq!(
        r["rowClosedAfterPress"], true,
        "one press with the row closed did not run, or opened the row: {r}"
    );
    assert_eq!(r["toggleOffAtStart"], true, "{r}");
    assert_eq!(
        r["toggleOffWhileBusy"], true,
        "the toggle did not follow Decide while it ran: {r}"
    );
    assert_eq!(r["toggleLiveWhenDone"], true, "{r}");
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

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn a_second_decision_supersedes_the_first() {
    let out = drive("double");
    let r = &out["report"];
    assert_eq!(out["parkRequests"], 2, "{out}");
    assert_eq!(out["streamRequests"], 2, "{out}");
    let events: Vec<&str> = out["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let at = |e: &str| events.iter().position(|x| *x == e);
    assert!(
        at("close 1").is_some() && at("close 1") < at("open 2"),
        "the first stream was still open when the second began: {events:?}"
    );
    assert_eq!(out["openStreams"], 0, "a stream was left open: {out}");
    // The first stream's `done` was due after the second began; none of it shows.
    let result = r["resultHtml"].as_str().unwrap();
    assert!(
        result.contains("(2)") && !result.contains("(1)"),
        "{result}"
    );
    assert_eq!(r["railRows"], 2, "{r}");
    assert_eq!(r["formBusy"], false, "{r}");
    no_errors(r);
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn stop_before_the_stream_opens_keeps_it_from_opening() {
    let out = drive("stop");
    let r = &out["report"];
    assert_eq!(out["parkRequests"], 1, "{out}");
    assert_eq!(
        out["streamRequests"], 0,
        "the stopped decision opened its stream anyway: {out}"
    );
    assert!(
        r["status"].as_str().unwrap().starts_with("stopped after"),
        "{r}"
    );
    assert_eq!(r["formBusy"], false, "{r}");
    no_errors(r);
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn an_error_frame_is_shown_as_its_sentence() {
    let out = drive("error");
    let r = &out["report"];
    let html = r["resultHtml"].as_str().unwrap();
    assert!(
        html.contains("the decision model is not answering.") && !html.contains("{"),
        "{html}"
    );
    assert_eq!(r["formBusy"], false, "{r}");
    no_errors(r);
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn a_row_that_cannot_be_sent_posts_nothing_and_says_why() {
    let out = drive("invalid");
    let r = &out["report"];
    assert_eq!(out["parkRequests"], 0, "{out}");
    assert_eq!(r["problemText"], "Name at least two options", "{r}");
    assert_eq!(r["formBusy"], false, "{r}");
    no_errors(r);
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn the_toggle_opens_the_row_and_its_own_decide_runs_what_it_shows() {
    let out = drive("row");
    let r = &out["report"];
    assert_eq!(r["toggleLive"], true, "{r}");
    assert_eq!(r["opened"], true, "the toggle did not open the row: {r}");
    assert_eq!(r["focusAfterOpen"], "decide-kind", "{r}");
    assert_eq!(r["optionsShown"], true, "{r}");
    assert_eq!(
        out["bodies"][0]["body"],
        r#"{"q":"Which one?","questions":{"verdict":{"type":"choice","criteria":{"keep":"stay with it","switch":""}}}}"#,
        "the row's Decide did not run the options: {out}"
    );
    assert_eq!(r["statusAfterRow"], "The decision is ready.", "{r}");
    assert_eq!(r["closed"], true, "the toggle did not close the row: {r}");
    assert_eq!(
        r["boxFocusedOnClose"], false,
        "a pressed toggle sent the focus to the box: {r}"
    );
    assert_eq!(
        out["bodies"][1]["body"], r#"{"q":"Which one?"}"#,
        "a closed row did not return the shape to yes / no: {out}"
    );
    assert_eq!(out["parkRequests"], 2, "{out}");
    assert_eq!(out["streamRequests"], 2, "{out}");
    assert_eq!(out["openStreams"], 0, "{out}");
    assert_eq!(r["toggleLiveAgain"], true, "{r}");
    assert_eq!(r["reopened"], true, "{r}");
    assert_eq!(r["kindAfterReopen"], "choice", "{r}");
    assert_eq!(r["optionsShownAfterReopen"], true, "{r}");
    assert_eq!(r["optionsAfterReopen"][0], "keep", "{r}");
    assert_eq!(r["optionsAfterReopen"][1], "switch", "{r}");
    assert_eq!(
        r["descAfterReopen"], "stay with it",
        "what was typed did not wait: {r}"
    );
    assert_eq!(r["formBusy"], false, "{r}");
    no_errors(r);
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn a_rating_sends_one_to_n_with_its_ends_named() {
    let out = drive("rating");
    let r = &out["report"];
    assert_eq!(r["ratingShown"], true, "{r}");
    assert_eq!(r["topAtStart"], "5", "{r}");
    assert_eq!(r["labelAtStart"], "5", "{r}");
    assert_eq!(
        out["bodies"][0]["body"],
        r#"{"q":"How sure is it?","questions":{"verdict":{"type":"score","criteria":["1","2","3","4","5"]}}}"#,
        "{out}"
    );
    assert_eq!(
        r["labelAfterSeven"], "7",
        "the second label did not follow the top: {r}"
    );
    assert_eq!(
        out["bodies"][1]["body"],
        r#"{"q":"How sure is it?","questions":{"verdict":{"type":"score","criteria":["1 (not at all)","2","3","4","5","6","7 (completely)"]}}}"#,
        "{out}"
    );
    assert_eq!(
        r["topAfterChange"], "10",
        "a settled top was not held to ten: {r}"
    );
    assert_eq!(r["labelAfterZero"], "2", "{r}");
    assert_eq!(
        r["topWhileTyping"], "0",
        "the field was rewritten under the typing: {r}"
    );
    assert_eq!(
        r["topAfterEnter"], "10",
        "Enter in the field ran it unsettled: {r}"
    );
    assert_eq!(
        out["bodies"][2]["body"],
        r#"{"q":"How sure is it?","questions":{"verdict":{"type":"score","criteria":["1 (not at all)","2","3","4","5","6","7","8","9","10 (completely)"]}}}"#,
        "{out}"
    );
    assert_eq!(out["parkRequests"], 3, "{out}");
    assert_eq!(out["openStreams"], 0, "{out}");
    assert!(
        r["resultHtml"]
            .as_str()
            .unwrap()
            .contains("10 (completely)"),
        "the bars do not carry the levels' names: {r}"
    );
    no_errors(r);
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn the_whole_form_works_from_the_keyboard() {
    let out = drive("keyboard");
    let r = &out["report"];
    assert_eq!(
        r["rowOpenAfterChord"], false,
        "Alt+Enter opened the row instead of deciding: {r}"
    );
    assert_eq!(
        out["bodies"][0]["body"], r#"{"q":"Which one?"}"#,
        "Alt+Enter with the row closed is yes / no: {out}"
    );
    assert_eq!(r["statusAfterChord"], "The decision is ready.", "{r}");
    let to_toggle: Vec<&str> = r["tabsToToggle"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        to_toggle,
        ["verb-ask", "verb-decide", "toggle"],
        "the toggle is not the stop after Decide"
    );
    assert_eq!(
        r["openedByToggle"], true,
        "Enter on the toggle did not open the row: {r}"
    );
    assert_eq!(r["expandedAfterOpen"], "true", "{r}");
    assert_eq!(r["focusAfterOpen"], "decide-kind", "{r}");
    assert_eq!(r["kindAfterArrow"], "choice", "{r}");
    assert_eq!(r["optionsShown"], true, "{r}");
    let tabs: Vec<&str> = r["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        tabs,
        [
            "decide-option",
            "decide-desc",
            "decide-option",
            "decide-desc",
            "add-option",
            "verb-decide-row"
        ],
        "Tab does not go name, meaning, name, meaning, add, Decide"
    );
    assert_eq!(r["focusBeforeEnter"], "decide-option", "{r}");
    assert_eq!(
        out["bodies"][1]["body"],
        r#"{"q":"Which one?","questions":{"verdict":{"type":"choice","criteria":{"keep":"","switch":""}}}}"#,
        "Enter in a name did not run it: {out}"
    );
    assert_eq!(r["statusAfterEnter"], "The decision is ready.", "{r}");
    assert_eq!(r["closedByEscape"], true, "{r}");
    assert_eq!(r["expandedAfterEscape"], "false", "{r}");
    assert_eq!(
        r["focusAfterEscape"], "q",
        "focus did not go back to the box: {r}"
    );
    assert_eq!(
        r["rowOpenAfterSecondChord"], false,
        "Alt+Enter reopened the row: {r}"
    );
    assert_eq!(
        out["bodies"][2]["body"], r#"{"q":"Which one?"}"#,
        "a row closed by Escape did not return the shape to yes / no: {out}"
    );
    assert_eq!(out["parkRequests"], 3, "{out}");
    assert_eq!(r["reopened"], true, "{r}");
    assert_eq!(r["optionsAfterReopen"][0], "keep", "{r}");
    assert_eq!(r["optionsAfterReopen"][1], "switch", "{r}");
    assert_eq!(r["optionsShownAfterReopen"], true, "{r}");
    assert_eq!(out["openStreams"], 0, "{out}");
    no_errors(r);
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn nothing_runs_off_a_phone() {
    let out = drive("narrow");
    let r = &out["report"];
    assert_eq!(r["optionCount"], 8, "{r}");
    assert_eq!(
        r["addDisabled"], true,
        "the add button outlived the limit: {r}"
    );
    for at in ["rowOpen", "rating", "result"] {
        let m = &r[at];
        assert!(
            m["scrollWidth"].as_u64() <= m["clientWidth"].as_u64(),
            "{at}: the page scrolls sideways at 390 px: {m}"
        );
        assert_eq!(
            m["small"].as_array().unwrap().len(),
            0,
            "{at}: targets under 44 px: {m}"
        );
    }
    assert!(
        r["decideHeight"].as_f64().unwrap() >= 44.0,
        "the Decide button is under 44 px: {r}"
    );
    assert!(
        r["toggleWidth"].as_f64().unwrap() >= 44.0 && r["toggleHeight"].as_f64().unwrap() >= 44.0,
        "the toggle is under 44 px: {r}"
    );
    assert_eq!(
        r["stacked"], true,
        "an option's two fields do not stack: {r}"
    );
    assert_eq!(r["railRows"], 8, "{r}");
    assert!(
        r["resultHtml"]
            .as_str()
            .unwrap()
            .contains("option 8 with a name forty characters"),
        "{r}"
    );
    no_errors(r);
}

#[test]
#[ignore = "needs node and a headless Chrome; see the module comment"]
fn enter_in_the_row_presses_decide_and_respects_its_state() {
    let out = drive("enter");
    let r = &out["report"];
    assert_eq!(
        out["parkRequests"], 0,
        "Enter went around a disabled Decide: {out}"
    );
    assert_eq!(
        r["rowDecideOffWhenStaged"], true,
        "the row's Decide outlived a staged file: {r}"
    );
    assert_eq!(r["problemWhenStaged"], "", "{r}");
    assert_eq!(r["problemText"], "Type a question first", "{r}");
    assert_eq!(
        r["toggleLiveWhileOpen"], true,
        "an open row could not be closed over an empty box: {r}"
    );
    assert_eq!(r["closed"], true, "{r}");
    assert_eq!(
        r["toggleOffOnceClosed"], true,
        "closed over an empty box, the toggle did not follow Decide: {r}"
    );
    no_errors(r);
}
