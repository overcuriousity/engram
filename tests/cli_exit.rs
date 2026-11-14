//! The binary's exit codes for a client verb refused before any request: `2`, one sentence, nothing on stdout.

#![allow(clippy::unwrap_used, clippy::expect_used)]

fn engram(argv: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_engram"))
        .args(argv)
        .env("ENGRAM_URL", "http://127.0.0.1:9")
        .env("ENGRAM_TOKEN", "engram_x")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the binary runs")
}

#[test]
fn a_refused_decide_flag_exits_two_in_one_sentence() {
    for argv in [
        &["-d", "Keep it?", "--option", "=x", "--option", "b"][..],
        &["-d", "Keep it?", "--option", "", "--option", "b"],
        &["-d", "-", "--situation", "-"],
    ] {
        let out = engram(argv);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{argv:?}: {err}");
        assert!(out.stdout.is_empty(), "{argv:?}");
        assert_eq!(err.trim().lines().count(), 1, "{argv:?}: {err}");
        assert!(!err.starts_with("Error:"), "{argv:?}: {err}");
        assert!(!err.contains("validation:"), "{argv:?}: {err}");
    }
}
