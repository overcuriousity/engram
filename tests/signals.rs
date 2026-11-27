//! What ends the server: a terminal sends SIGINT, systemd stops a unit with SIGTERM.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

#[tokio::test]
async fn a_sigterm_ends_the_server_as_a_ctrl_c_does() {
    let ended = tokio::spawn(engram::web::shutdown_signal());
    // The handler is installed on first poll; a signal sent before that kills the test.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let me = std::process::id().to_string();
    let sent = std::process::Command::new("kill")
        .args(["-TERM", &me])
        .status()
        .unwrap();
    assert!(sent.success());
    tokio::time::timeout(Duration::from_secs(5), ended)
        .await
        .expect("SIGTERM was not heard")
        .unwrap();
}
