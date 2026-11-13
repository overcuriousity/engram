#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::io::Write;
use std::os::unix::net::UnixDatagram;
use std::time::Duration;

#[test]
fn ready_is_sent_once_listening() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("notify.sock");
    let listener = UnixDatagram::bind(&sock).unwrap();
    listener
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    let hash = engram::auth::local::hash_password("pw").unwrap();
    let cfg = dir.path().join("config.toml");
    let mut f = std::fs::File::create(&cfg).unwrap();
    write!(
        f,
        r#"
[server]
bind = "127.0.0.1:0"
[store]
control_path = "{d}/control.db"
dir = "{d}/users"
[vector]
url = "http://127.0.0.1:9"
collection = "notify"
[infer.embed]
base_url = "http://127.0.0.1:9/v1"
model = "m"
dim = 8
max_input_tokens = 512
[auth]
mode = "local"
[auth.local]
username = "dev"
password_hash = "{hash}"
"#,
        d = dir.path().display()
    )
    .unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_engram"))
        .arg("--config")
        .arg(&cfg)
        .current_dir(dir.path())
        .env("NOTIFY_SOCKET", &sock)
        .env_remove("WATCHDOG_USEC")
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(dir.path().join("stderr.log")).unwrap())
        .spawn()
        .unwrap();
    let mut buf = [0u8; 256];
    let got = listener.recv(&mut buf);
    let _ = child.kill();
    let _ = child.wait();
    let n = got.unwrap_or_else(|e| {
        panic!(
            "no notification arrived: {e}: {}",
            std::fs::read_to_string(dir.path().join("stderr.log")).unwrap_or_default()
        )
    });
    let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(msg.contains("READY=1"), "{msg}");
}
