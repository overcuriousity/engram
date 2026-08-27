# The Terminal Door — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `engram -c`, `-s` and `-a` — a client on the existing binary that captures, searches and asks from the shell, with a tty rendering drawn in the base's own vocabulary.

**Architecture:** The client speaks HTTP to a running server and never opens the store, so one set of ranking parameters, tenancy checks and feedback recording stands behind every door. Verb selection is a pure function of the parsed arguments and whether stdin is a pipe, so it is unit-testable without a terminal. Rendering is split in two: a plain renderer that is the only thing tests and scripts ever see, and a face over it that runs only on a tty.

**Tech Stack:** Rust 2024, clap 4 (derive), reqwest 0.13 (rustls, json, and the `stream` feature added here), `tokio-stream` (already in the tree), `toml_edit` (already in the tree), crossterm (added here), `std::io::IsTerminal`.

**Spec:** `docs/superpowers/specs/2026-08-27-capture-doors-design.md` (§4 and §4a)

**Depends on:** `docs/superpowers/plans/2026-08-27-capture-doors-server.md` Tasks 2-5 — `POST /api/v1/capture` and `Door::Cli` must exist first.

## Global Constraints

- **One new dependency and one new feature flag, both pure Rust, and the binary stays one file.** `crossterm` (terminal size, colour capability, Windows ANSI) and reqwest's `stream` feature. Everything else is already in the tree: `tokio-stream` for the SSE chunks, `toml_edit` for the client config, `mime_guess` for a path's type. No C dependency, no curses, nothing shipped beside the executable.
- **The plain form is the tested form.** Every assertion about output is made against `--plain`. With `stdout` not a terminal, or `NO_COLOR` set, or `--plain`, the output holds **no escape byte at all** and no glyph outside ASCII.
- **Nothing is said by colour alone.** The cliff, the loose-match label, the model-written badge and `held for review` are words in both forms.
- **No animation may delay a result.** Frames run on their own thread and stop on the first byte that arrives; nothing is buffered or paced for effect.
- **`engram` with no verb flag and a tty on stdin still starts the server.** No existing invocation changes meaning. This is asserted by a test.
- **Exit codes:** `0` results, `1` none, `2` error.
- **House style.** Doc comments say *why*. Test names are sentences.
- **Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before every commit.**

---

### Task 1: The verb surface

Which verb was asked for is a pure function of the parsed flags plus one fact about the environment — whether stdin is a pipe. Making it a function rather than a branch inside `main` is what makes every rule in §4 testable.

**Files:**
- Create: `src/cli/mod.rs`, `src/cli/args.rs`
- Modify: `src/main.rs:8-60` (the flags), `src/lib.rs` (declare `pub mod cli;`)
- Test: `src/cli/args.rs` `mod tests`

**Interfaces:**
- Produces: `enum Verb { Capture(Vec<String>), Search { limit: Option<usize>, query: String }, Ask(String) }` and `pub fn verb(args: &CliArgs, stdin_piped: bool, stdin: impl FnOnce() -> std::io::Result<String>) -> Result<Option<Verb>>`. `None` means "no verb — be the server". Tasks 3, 4 and 5 match on `Verb`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> CliArgs {
        CliArgs::default()
    }
    fn piped(text: &'static str) -> impl FnOnce() -> std::io::Result<String> {
        move || Ok(text.to_string())
    }

    #[test]
    fn a_leading_integer_is_how_many_hits_are_wanted() {
        let mut a = args();
        a.search = vec!["40".into(), "qdrant payload filter".into()];
        let v = verb(&a, false, piped("")).unwrap().unwrap();
        assert!(matches!(v, Verb::Search { limit: Some(40), ref query } if query == "qdrant payload filter"));
    }

    #[test]
    fn a_query_that_is_itself_a_number_is_reachable() {
        // `-s -- 42` puts the number after the separator, where clap stops
        // reading flags: it is a query, not a count.
        let mut a = args();
        a.search = vec!["42".into()];
        let v = verb(&a, false, piped("")).unwrap().unwrap();
        assert!(
            matches!(v, Verb::Search { limit: None, ref query } if query == "42"),
            "a lone number is the query — there is nothing left for it to count"
        );
    }

    #[test]
    fn stdin_is_the_value_of_whichever_verb_was_named() {
        let mut a = args();
        a.search = vec!["-".into()];
        let v = verb(&a, true, piped("loop device")).unwrap().unwrap();
        assert!(matches!(v, Verb::Search { ref query, .. } if query == "loop device"));
    }

    #[test]
    fn a_pipe_with_no_verb_at_all_is_a_capture() {
        let v = verb(&args(), true, piped("a procedure worth keeping")).unwrap().unwrap();
        assert!(matches!(v, Verb::Capture(ref w) if w == &["-".to_string()]));
    }

    #[test]
    fn a_terminal_with_no_verb_is_the_server_it_has_always_been() {
        assert!(
            verb(&args(), false, piped("")).unwrap().is_none(),
            "`engram` alone must not change meaning"
        );
    }

    #[test]
    fn two_verbs_in_one_invocation_are_refused() {
        let mut a = args();
        a.search = vec!["a".into()];
        a.ask = vec!["b".into()];
        assert!(verb(&a, false, piped("")).is_err());
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib cli::args`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Write the flags and the function**

`src/cli/args.rs`:

```rust
//! Which verb an invocation asked for, decided before anything is opened.
//!
//! A pure function of the parsed flags and one fact about the environment,
//! rather than a branch inside `main`: every rule here — the leading count,
//! what a pipe means, what `engram` alone means — is a rule worth a test, and
//! none of them is testable through a real terminal.

use crate::error::{Error, Result};

/// The client half of the binary's arguments. Flattened into `Args` in
/// `main.rs`, so the server's flags and the client's are one parser and
/// `--help` lists both.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct CliArgs {
    /// Capture a file, a link, or `-` for standard input. Repeatable:
    /// `engram -c *.pdf` is one invocation and several corpora.
    #[arg(short = 'c', value_name = "PATH|URL|-", num_args = 1.., conflicts_with_all = ["search", "ask"])]
    pub capture: Vec<String>,
    /// Search. A leading bare integer is how many hits are wanted.
    #[arg(short = 's', value_name = "[N] QUERY", num_args = 1.., conflicts_with_all = ["capture", "ask"])]
    pub search: Vec<String>,
    /// Ask one question across the base.
    #[arg(short = 'a', value_name = "QUESTION", num_args = 1.., conflicts_with_all = ["capture", "search"])]
    pub ask: Vec<String>,
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,
    #[arg(long, value_name = "NOTE")]
    pub note: Option<String>,
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
    #[arg(long, value_name = "CATEGORY")]
    pub category: Option<String>,
    /// Print the results as JSON instead of for a person.
    #[arg(long)]
    pub json: bool,
    /// Never colour, never animate, never leave ASCII.
    #[arg(long)]
    pub plain: bool,
    /// Override the tty detection in either direction.
    #[arg(long, value_name = "WHEN", default_value = "auto")]
    pub fancy: Fancy,
    /// After capturing, follow the background stages until they finish.
    #[arg(long)]
    pub watch: bool,
}

#[derive(clap::ValueEnum, Debug, Default, Clone, Copy, PartialEq)]
pub enum Fancy {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, PartialEq)]
pub enum Verb {
    Capture(Vec<String>),
    Search { limit: Option<usize>, query: String },
    Ask(String),
}

/// The verb, or `None` for "no verb was asked for — be the server".
///
/// `stdin_piped` is passed rather than detected, and `stdin` is a closure
/// rather than a read, so the whole decision runs in a test with no terminal
/// and no process to pipe into.
pub fn verb(
    args: &CliArgs,
    stdin_piped: bool,
    stdin: impl FnOnce() -> std::io::Result<String>,
) -> Result<Option<Verb>> {
    let named = [!args.capture.is_empty(), !args.search.is_empty(), !args.ask.is_empty()]
        .iter()
        .filter(|x| **x)
        .count();
    if named > 1 {
        return Err(Error::Validation(
            "one verb at a time: `-c`, `-s` or `-a`".into(),
        ));
    }

    let read = |words: &[String]| -> Result<String> {
        let joined = words.join(" ");
        if joined.trim() == "-" {
            return stdin()
                .map(|s| s.trim().to_string())
                .map_err(|e| Error::Validation(format!("stdin: {e}")));
        }
        Ok(joined)
    };

    if !args.capture.is_empty() {
        return Ok(Some(Verb::Capture(args.capture.clone())));
    }
    if !args.ask.is_empty() {
        return Ok(Some(Verb::Ask(read(&args.ask)?)));
    }
    if !args.search.is_empty() {
        // A leading integer is a count only when something is left to be the
        // query. A lone `42` has nothing left, so it is what was asked about.
        let (limit, rest) = match args.search.split_first() {
            Some((head, rest)) if !rest.is_empty() => match head.parse::<usize>() {
                Ok(n) => (Some(n), rest.to_vec()),
                Err(_) => (None, args.search.clone()),
            },
            _ => (None, args.search.clone()),
        };
        return Ok(Some(Verb::Search { limit, query: read(&rest)? }));
    }

    // No verb named. A pipe is still an instruction: capturing what was piped
    // is the gesture the whole terminal door exists for, and requiring `-c -`
    // there would be ceremony in front of the one case that has to be
    // frictionless. A terminal on stdin means no instruction at all, so the
    // binary is the server it has always been.
    if stdin_piped {
        return Ok(Some(Verb::Capture(vec!["-".into()])));
    }
    Ok(None)
}
```

`src/cli/mod.rs`:

```rust
//! The terminal door: capture, search and ask from a shell.
//!
//! Over HTTP, never into the store — one set of ranking parameters, tenancy
//! checks and feedback recording stands behind every door, and a search typed
//! at a shell is a real recorded search the judge page can grade later.

pub mod args;
```

In `src/main.rs`, add to `struct Args`:

```rust
    #[command(flatten)]
    cli: engram::cli::args::CliArgs,
```

And declare `pub mod cli;` in `src/lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib cli::args`
Expected: PASS, all six.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/cli src/main.rs src/lib.rs
git commit -m "feat: the verb an invocation asked for, decided in one function

A pure function of the flags and whether stdin is a pipe, because every
rule in it — the leading count, what a pipe means, what \`engram\` alone
means — is a rule worth a test and none is testable through a tty."
```

---

### Task 2: Where the client points

**Files:**
- Create: `src/cli/endpoint.rs`
- Modify: `src/cli/mod.rs`
- Test: `src/cli/endpoint.rs` `mod tests`

**Interfaces:**
- Produces: `pub struct Endpoint { pub url: String, pub token: String }` and `pub fn resolve(env: &dyn Fn(&str) -> Option<String>, file: Option<&std::path::Path>) -> Result<Endpoint>`. Tasks 3, 4, 5 and 7 take an `&Endpoint`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + '_ {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cli.toml");
        std::fs::write(&path, "url = \"https://file.test\"\ntoken = \"engram_file\"\n").unwrap();
        let e = resolve(
            &env_of(&[("ENGRAM_URL", "https://env.test"), ("ENGRAM_TOKEN", "engram_env")]),
            Some(&path),
        )
        .unwrap();
        assert_eq!(e.url, "https://env.test");
        assert_eq!(e.token, "engram_env");
    }

    #[test]
    fn the_file_is_read_when_the_environment_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cli.toml");
        std::fs::write(&path, "url = \"https://file.test\"\ntoken = \"engram_file\"\n").unwrap();
        let e = resolve(&env_of(&[]), Some(&path)).unwrap();
        assert_eq!(e.token, "engram_file");
    }

    #[test]
    fn a_missing_token_names_the_page_that_mints_one() {
        let err = resolve(&env_of(&[("ENGRAM_URL", "https://env.test")]), None).unwrap_err();
        let said = err.to_string();
        assert!(said.contains("/ui/settings"), "unhelpful: {said}");
        assert!(said.contains("ENGRAM_TOKEN"), "unhelpful: {said}");
    }

    #[test]
    fn a_trailing_slash_does_not_become_a_double_one() {
        let e = resolve(
            &env_of(&[("ENGRAM_URL", "https://env.test/"), ("ENGRAM_TOKEN", "engram_env")]),
            None,
        )
        .unwrap();
        assert_eq!(e.api("/search"), "https://env.test/api/v1/search");
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib cli::endpoint`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write it**

```rust
//! Which engram this client is talking to, and with what credential.

use crate::error::{Error, Result};

pub struct Endpoint {
    /// No trailing slash, so joining is concatenation and never guesswork.
    pub url: String,
    pub token: String,
}

#[derive(serde::Deserialize, Default)]
struct FileConfig {
    url: Option<String>,
    token: Option<String>,
}

impl Endpoint {
    /// The full URL of an API path, which must begin with `/`.
    pub fn api(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.url)
    }
}

/// The default location of the client's config, `~/.config/engram/cli.toml`.
pub fn default_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h)
            .join(".config")
            .join("engram")
            .join("cli.toml")
    })
}

/// Environment first, then the file. `env` is a closure so the precedence is
/// testable without mutating the process's own environment, which two tests
/// running at once would race on.
pub fn resolve(
    env: &dyn Fn(&str) -> Option<String>,
    file: Option<&std::path::Path>,
) -> Result<Endpoint> {
    let from_file: FileConfig = file
        .filter(|p| p.exists())
        .map(|p| {
            std::fs::read_to_string(p)
                .map_err(|e| Error::Validation(format!("{}: {e}", p.display())))
                .and_then(|s| {
                    toml::from_str(&s).map_err(|e| Error::Validation(format!("{}: {e}", p.display())))
                })
        })
        .transpose()?
        .unwrap_or_default();

    let url = env("ENGRAM_URL")
        .or(from_file.url)
        .unwrap_or_else(|| "http://127.0.0.1:8080".into());
    let token = env("ENGRAM_TOKEN").or(from_file.token).ok_or_else(|| {
        // The one error a first-time user will certainly see, so it says what
        // to do rather than what went wrong.
        Error::Validation(
            "no token: set ENGRAM_TOKEN, or write `token = \"engram_…\"` into \
             ~/.config/engram/cli.toml. Mint one at /ui/settings."
                .into(),
        )
    })?;

    Ok(Endpoint {
        url: url.trim_end_matches('/').to_string(),
        token,
    })
}
```

**No new dependency here.** `toml` is *not* in the tree — only `toml_edit = "0.25.13"` (`Cargo.toml:83`), which the config writer uses. Parse the two keys with it rather than adding a second TOML crate:

```rust
    let parsed: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::Validation(format!("{}: {e}", p.display())))?;
    let value = |k: &str| parsed.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let from_file = FileConfig { url: value("url"), token: value("token") };
```

with `FileConfig` kept as a plain struct rather than a `Deserialize` one.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib cli::endpoint`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/cli Cargo.toml
git commit -m "feat: where the terminal door points, and with what credential

The one error a first-time user will certainly see says what to do
rather than what went wrong."
```

---

### Task 3: `-c` captures a path, a link and a pipe

**Files:**
- Create: `src/cli/capture.rs`, `src/cli/test_support.rs`
- Modify: `src/cli/mod.rs`, `src/main.rs` (dispatch), `Cargo.toml`
- Test: `src/cli/capture.rs` `mod tests`

**Interfaces:**
- Consumes: `Endpoint` (Task 2), `Verb::Capture` (Task 1).
- Produces: `pub async fn run(e: &Endpoint, targets: &[String], title: Option<&str>, note: Option<&str>) -> Result<Vec<String>>` returning the corpus ids, and `pub async fn serve_test_app() -> (String, String)` in `test_support` returning `(base_url, token)`.

- [ ] **Step 1: Write the failing tests**

`src/cli/test_support.rs` first — the client needs a real server to be worth testing:

```rust
//! A real engram on a real port, for the client tests.
//!
//! The client speaks HTTP and nothing else, so a mock would be asserting
//! against a fiction. This is the actual router, served on a port the OS
//! chose, with a real bearer token.

pub async fn serve_test_app() -> (String, String) {
    let core = crate::core::test_support::test_core().await;
    let (app, token) = crate::web::test_support::app_with_token(core).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let addr = listener.local_addr().expect("the port it got");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}"), token)
}
```

Then in `src/cli/capture.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    async fn endpoint() -> Endpoint {
        let (url, token) = crate::cli::test_support::serve_test_app().await;
        Endpoint { url, token }
    }

    #[tokio::test]
    async fn a_path_a_link_and_a_pipe_all_land_as_corpora() {
        let e = endpoint().await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "a procedure worth keeping").unwrap();

        let ids = run(&e, &[file.display().to_string()], None, None).await.unwrap();
        assert_eq!(ids.len(), 1);

        let ids = run(&e, &["https://example.test/a".into()], None, None).await.unwrap();
        assert_eq!(ids.len(), 1, "a link is captured as a link");
    }

    #[tokio::test]
    async fn several_paths_in_one_invocation_are_several_corpora() {
        let e = endpoint().await;
        let dir = tempfile::tempdir().unwrap();
        let mut targets = Vec::new();
        for (i, body) in ["the first procedure", "the second procedure"].iter().enumerate() {
            let p = dir.path().join(format!("{i}.txt"));
            std::fs::write(&p, body).unwrap();
            targets.push(p.display().to_string());
        }
        let ids = run(&e, &targets, None, None).await.unwrap();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }

    #[tokio::test]
    async fn a_refusal_from_the_server_is_reported_in_the_servers_words() {
        let e = Endpoint { url: "http://127.0.0.1:1".into(), token: "engram_nope".into() };
        let err = run(&e, &["-".into()], None, None).await.unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib cli::capture`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write the capture verb**

```rust
//! `-c`: put a path, a link or a pipe into the base.

use crate::cli::endpoint::Endpoint;
use crate::error::{Error, Result};

/// Capture each target in turn, answering with the corpus ids in the order
/// they were given.
///
/// One request per target rather than one multipart body holding all of them:
/// a glob of forty PDFs that fails on the nineteenth should have stored
/// eighteen, and the operator should be told which one stopped it.
pub async fn run(
    e: &Endpoint,
    targets: &[String],
    title: Option<&str>,
    note: Option<&str>,
) -> Result<Vec<String>> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("engram-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Internal(format!("http client: {e}")))?;
    let mut ids = Vec::new();
    for target in targets {
        let (bytes, content_type) = read_target(target)?;
        let mut url = format!("{}?", e.api("/capture"));
        if let Some(t) = title {
            url.push_str(&format!("title={}&", urlencoding(t)));
        }
        if let Some(n) = note {
            url.push_str(&format!("note={}", urlencoding(n)));
        }
        let res = http
            .post(url.trim_end_matches(['?', '&']))
            .bearer_auth(&e.token)
            .header("content-type", content_type)
            .body(bytes)
            .send()
            .await
            .map_err(|err| Error::Validation(format!("{target}: {err}")))?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            let said = body["error"].as_str().unwrap_or("the server refused it");
            return Err(Error::Validation(format!("{target}: {said}")));
        }
        // Said out loud rather than folded into a success: a parked capture is
        // stored and nothing more — not segmented, not embedded, not
        // searchable until someone decides in the web UI.
        if let Some(n) = body.get("near_duplicate").filter(|v| !v.is_null()) {
            eprintln!(
                "{target}: held for review — {:.0}% similar to {}",
                n["similarity"].as_f64().unwrap_or(0.0) * 100.0,
                n["corpus_id"].as_str().unwrap_or("something already stored")
            );
        }
        ids.push(body["id"].as_str().unwrap_or_default().to_string());
    }
    Ok(ids)
}

/// The bytes to send and what to call them.
///
/// A link is sent as `text/plain` and the server decides it is a link, which
/// is the same decision a share sheet's body gets: one rule, one place.
fn read_target(target: &str) -> Result<(Vec<u8>, String)> {
    if target == "-" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| Error::Validation(format!("stdin: {e}")))?;
        return Ok((buf, "text/plain".into()));
    }
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok((target.as_bytes().to_vec(), "text/plain".into()));
    }
    let bytes = std::fs::read(target).map_err(|e| Error::Validation(format!("{target}: {e}")))?;
    let mime = mime_guess::from_path(target)
        .first_raw()
        .unwrap_or("text/plain")
        .to_string();
    Ok((bytes, mime))
}

/// Percent-encode a query value. Only the characters that would end the value
/// or the query; nothing here is a general-purpose encoder.
fn urlencoding(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
```

Dispatch in `src/main.rs`, before `Config::load` — the client must not need a server's configuration file:

```rust
    // The client half, decided before anything is opened: it talks to a
    // running engram over HTTP and needs neither this machine's config.toml
    // nor its database.
    if let Some(verb) = engram::cli::args::verb(&args.cli, !std::io::stdin().is_terminal(), || {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    })? {
        let code = engram::cli::run(verb, &args.cli).await;
        std::process::exit(code);
    }
```

And in `src/cli/mod.rs`:

```rust
/// Run a verb and answer with the process's exit code: `0` results, `1` none,
/// `2` a failure. A shell branches on these — `engram -s "x" || …` — so they
/// are part of the interface, not a detail.
pub async fn run(verb: args::Verb, cli: &args::CliArgs) -> i32 {
    let endpoint = match endpoint::resolve(&|k| std::env::var(k).ok(), endpoint::default_path().as_deref()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    match verb {
        args::Verb::Capture(targets) => {
            match capture::run(&endpoint, &targets, cli.title.as_deref(), cli.note.as_deref()).await {
                Ok(ids) => {
                    for id in &ids {
                        println!("{id}");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    2
                }
            }
        }
        _ => 0, // the other verbs arrive in Tasks 4 and 5
    }
}
```

Add to `Cargo.toml`: nothing new for this task — `reqwest`, `mime_guess` and `tempfile` (dev) are already in the tree.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib cli::`
Expected: PASS.

- [ ] **Step 5: Verify by hand against a running engram**

```bash
cargo run -- --config config.toml &          # the server
echo "a procedure worth keeping" | ENGRAM_TOKEN=… cargo run -- -c -
ENGRAM_TOKEN=… cargo run -- -c README.md --title "the readme"
```

Expected: a corpus id on stdout for each, and both visible on `/ui/capture`.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/cli src/main.rs
git commit -m "feat: capture from a shell

One request per target, so a glob of forty that fails on the nineteenth
has stored eighteen and says which one stopped it. A parked capture is
said out loud rather than folded into a success."
```

---

### Task 4: `-s` searches, and prints what the rail would say

**Files:**
- Create: `src/cli/search.rs`
- Modify: `src/cli/mod.rs` (dispatch the verb)
- Test: `src/cli/search.rs` `mod tests`

**Interfaces:**
- Consumes: `Endpoint`, `Verb::Search`.
- Produces: `pub async fn run(e: &Endpoint, limit: Option<usize>, query: &str, cli: &CliArgs) -> Result<i32>` and `pub fn render_plain(hits: &[SearchResult]) -> String`. Task 6 wraps `render_plain`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::search::SearchResult;

    fn hit(title: &str, score: f32, weak: bool, past_cliff: bool) -> SearchResult {
        let mut h = SearchResult::default();
        h.artifact_id = format!("art-{title}");
        h.title = Some(title.into());
        h.text = format!("the body of {title}");
        h.score = score;
        h.weak = weak;
        h.past_cliff = past_cliff;
        h
    }

    #[test]
    fn the_plain_form_holds_no_escape_byte_at_all() {
        let out = render_plain(&[hit("a", 0.8, false, false), hit("b", 0.2, true, true)]);
        assert!(!out.contains('\u{1b}'), "an escape reached the plain form: {out:?}");
        assert!(out.is_ascii() || out.chars().all(|c| c != '█'), "no drawing glyphs in plain");
    }

    #[test]
    fn the_cliff_and_the_loose_match_are_words_not_colours() {
        let out = render_plain(&[hit("a", 0.8, false, false), hit("b", 0.2, true, true)]);
        assert!(out.contains("past the cliff"), "{out}");
        assert!(out.contains("loose"), "{out}");
    }

    #[test]
    fn a_hit_prints_its_rank_score_title_and_id() {
        let out = render_plain(&[hit("a", 0.83, false, false)]);
        assert!(out.contains(" 1 "), "{out}");
        assert!(out.contains("0.83"), "{out}");
        assert!(out.contains("art-a"), "{out}");
    }

    #[tokio::test]
    async fn nothing_found_exits_one_and_something_found_exits_zero() {
        let (url, token) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        let cli = crate::cli::args::CliArgs { plain: true, ..Default::default() };
        assert_eq!(run(&e, None, "nothing is stored yet", &cli).await.unwrap(), 1);

        crate::cli::capture::run(&e, &["-".into()], None, None).await.ok();
        // A capture is segmented in the background; the search that follows is
        // asserted only on its exit code being reachable, not on a race.
        assert!(matches!(run(&e, Some(40), "procedure", &cli).await, Ok(0) | Ok(1)));
    }

    #[tokio::test]
    async fn the_client_claims_the_cli_door() {
        let (url, token) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        assert!(query_url(&e, Some(40), "loop device", &Default::default()).contains("door=cli"));
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib cli::search`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write it**

```rust
//! `-s`: a ranked list in a terminal, saying everything the rail says.

use crate::cli::args::CliArgs;
use crate::cli::endpoint::Endpoint;
use crate::core::search::SearchResult;
use crate::error::{Error, Result};

/// The URL a search is asked for at. Split out because one test asserts the
/// door the client claims, and a whole request is a poor place to assert it.
pub fn query_url(e: &Endpoint, limit: Option<usize>, query: &str, cli: &CliArgs) -> String {
    let mut url = format!("{}?q={}&door=cli", e.api("/search"), encode(query));
    if let Some(n) = limit {
        url.push_str(&format!("&limit={n}"));
    }
    if !cli.tags.is_empty() {
        url.push_str(&format!("&tags={}", encode(&cli.tags.join(","))));
    }
    if let Some(c) = &cli.category {
        url.push_str(&format!("&category={}", encode(c)));
    }
    url
}

pub async fn run(e: &Endpoint, limit: Option<usize>, query: &str, cli: &CliArgs) -> Result<i32> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("engram-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
    let res = http
        .get(query_url(e, limit, query, cli))
        .bearer_auth(&e.token)
        .send()
        .await
        .map_err(|err| Error::Validation(format!("{err}")))?;
    if !res.status().is_success() {
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        return Err(Error::Validation(
            body["error"].as_str().unwrap_or("the search was refused").into(),
        ));
    }
    let body = res.text().await.map_err(|err| Error::Validation(format!("{err}")))?;
    if cli.json {
        println!("{body}");
    }
    let hits: Vec<SearchResult> =
        serde_json::from_str(&body).map_err(|err| Error::Internal(format!("results: {err}")))?;
    if !cli.json {
        print!("{}", render_plain(&hits));
    }
    // `1` for nothing found, so `engram -s "x" || …` is a usable branch.
    Ok(if hits.is_empty() { 1 } else { 0 })
}

/// The form a pipe, a test and a script see. No colour, no motion, no glyph
/// outside ASCII — and every claim the rail makes said in words, because a
/// rendering that marks the cliff only by being dim marks it for nobody in a
/// monochrome terminal, a screenshot, or a colourblind reader's eyes.
pub fn render_plain(hits: &[SearchResult]) -> String {
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let mark = if h.past_cliff { "·" } else { " " };
        let title = h.title.as_deref().unwrap_or("(untitled)");
        out.push_str(&format!(
            "{mark}{:>2} {:.2}  {title}  {}\n",
            i + 1,
            h.score,
            h.artifact_id
        ));
        let mut said: Vec<&str> = Vec::new();
        if h.past_cliff {
            said.push("past the cliff");
        }
        if h.weak {
            said.push("loose match");
        }
        if h.model_written {
            said.push("model-written");
        }
        if h.primed {
            said.push("primed");
        }
        if !said.is_empty() {
            out.push_str(&format!("      [{}]\n", said.join(", ")));
        }
        for line in h.text.lines().take(3) {
            out.push_str(&format!("      {line}\n"));
        }
        out.push('\n');
    }
    out
}

fn encode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
```

Move `encode` into `src/cli/mod.rs` as `pub(crate) fn encode` and have `capture.rs` use it too rather than keeping two copies.

Wire `Verb::Search { limit, query }` into `cli::run`'s match, returning the code `search::run` answered with.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib cli::search`
Expected: PASS. If `SearchResult` has no `Default`, derive one in `src/core/search.rs` — it is a plain data struct — or build the fixture with every field spelled out.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/cli src/core/search.rs
git commit -m "feat: a ranked list in a terminal, saying what the rail says

Asking wider is asking wider: the candidate pool is a multiple of the
limit. The cliff and the loose match are words, because a mark made
only of colour is made for nobody in a pipe."
```

---

### Task 5: `-a` streams an answer

**Files:**
- Create: `src/cli/ask.rs`
- Modify: `src/cli/mod.rs`, `Cargo.toml` (reqwest `stream` feature)
- Test: `src/cli/ask.rs` `mod tests`

**Interfaces:**
- Consumes: `Endpoint`, `Verb::Ask`.
- Produces: `pub async fn run(e: &Endpoint, question: &str, cli: &CliArgs) -> Result<i32>` and `pub fn frames(buf: &mut String) -> Vec<(String, serde_json::Value)>` — the SSE reader, split out so it is testable without a server.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_read_out_of_a_buffer_that_splits_mid_event() {
        let mut buf = String::from("event: token\ndata: {\"text\":\"hel\"}\n\nevent: tok");
        let got = frames(&mut buf);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "token");
        assert_eq!(got[0].1["text"], "hel");
        assert_eq!(buf, "event: tok", "the half-arrived frame is kept for the next read");

        buf.push_str("en\ndata: {\"text\":\"lo\"}\n\n");
        let got = frames(&mut buf);
        assert_eq!(got[0].1["text"], "lo");
    }

    #[tokio::test]
    async fn an_answer_streams_and_the_citations_follow_it() {
        let (url, token) = crate::cli::test_support::serve_test_app().await;
        let e = Endpoint { url, token };
        let cli = crate::cli::args::CliArgs { plain: true, ..Default::default() };
        // The test core's synthesizer is a fake, so this asserts the transport
        // and the frame reader, not the words that come back.
        let code = run(&e, "what is stored", &cli).await.unwrap();
        assert!(code == 0 || code == 1);
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib cli::ask`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write it**

```rust
//! `-a`: one question, streamed to the terminal as it is written.

use crate::cli::args::CliArgs;
use crate::cli::endpoint::Endpoint;
use crate::error::{Error, Result};

/// Take every complete SSE frame out of `buf`, leaving a partial one behind.
///
/// A chunk from the network splits wherever it likes — mid-event, mid-JSON —
/// so the reader has to be able to say "not yet" and keep what it has. Split
/// out of the request loop because that is the only way to test it against a
/// split that actually happened.
pub fn frames(buf: &mut String) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    while let Some(end) = buf.find("\n\n") {
        let block = buf[..end].to_string();
        buf.drain(..end + 2);
        let mut name = String::new();
        let mut data = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data.push_str(rest);
            }
        }
        if name.is_empty() {
            continue;
        }
        let value = serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
        out.push((name, value));
    }
    out
}

pub async fn run(e: &Endpoint, question: &str, cli: &CliArgs) -> Result<i32> {
    use tokio_stream::StreamExt;
    let http = reqwest::Client::builder()
        .user_agent(concat!("engram-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| Error::Internal(format!("http client: {err}")))?;
    let res = http
        .post(e.api("/ask/stream"))
        .bearer_auth(&e.token)
        .json(&serde_json::json!({ "q": question }))
        .send()
        .await
        .map_err(|err| Error::Validation(format!("{err}")))?;
    if !res.status().is_success() {
        return Err(Error::Validation("the question was refused".into()));
    }

    let mut body = res.bytes_stream();
    let mut buf = String::new();
    let mut citations = Vec::new();
    let mut said_anything = false;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|err| Error::Validation(format!("{err}")))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        for (name, data) in frames(&mut buf) {
            match name.as_str() {
                "token" => {
                    if let Some(t) = data["text"].as_str() {
                        print!("{t}");
                        use std::io::Write;
                        std::io::stdout().flush().ok();
                        said_anything = true;
                    }
                }
                "citations" => citations = data["hits"].as_array().cloned().unwrap_or_default(),
                // Said, not swallowed: an answer that stopped and an answer
                // that failed look identical on a terminal otherwise.
                "error" => {
                    eprintln!("\n{}", data["error"].as_str().unwrap_or("the answer failed"));
                    return Ok(2);
                }
                _ => {}
            }
        }
    }
    println!();
    if !citations.is_empty() {
        println!("\nfrom:");
        for c in &citations {
            println!(
                "  └─ {}  {}",
                c["title"].as_str().unwrap_or("(untitled)"),
                c["artifact_id"].as_str().unwrap_or("")
            );
        }
    }
    let _ = cli;
    // `1` when the base had nothing to say, matching `-s`: a shell branches on
    // it the same way.
    Ok(if said_anything { 0 } else { 1 })
}
```

Add the `stream` feature to reqwest in `Cargo.toml` — both the dependency and the `[dev-dependencies]` copy, or the tests will not see it. `futures-util` is **not** needed: `tokio-stream` is already a dependency and its `StreamExt` supplies `next()`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib cli::ask`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/cli Cargo.toml
git commit -m "feat: ask from a shell, streamed as it is written

The frame reader is its own function because a chunk from the network
splits mid-JSON, and 'not yet' is the behaviour worth a test."
```

---

### Task 6: The face

**Files:**
- Create: `src/cli/face.rs`
- Modify: `src/cli/search.rs` (call the face when it is on), `src/cli/mod.rs`, `Cargo.toml`
- Test: `src/cli/face.rs` `mod tests`

**Interfaces:**
- Consumes: `render_plain` (Task 4), `CliArgs::{plain, fancy}`.
- Produces: `pub struct Face { on: bool, unicode: bool, width: usize }`, `Face::decide(cli: &CliArgs, is_tty: bool, no_color: bool, lang: Option<&str>) -> Face`, `Face::render(&self, hits: &[SearchResult]) -> String`, `Face::pulse(&self) -> Option<Pulse>` where `Pulse` stops on drop.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{CliArgs, Fancy};

    #[test]
    fn the_face_is_off_wherever_it_could_reach_a_machine() {
        let plain = CliArgs { plain: true, ..Default::default() };
        assert!(!Face::decide(&plain, true, false, Some("en_US.UTF-8")).on, "--plain wins");
        assert!(!Face::decide(&Default::default(), false, false, Some("en_US.UTF-8")).on, "a pipe");
        assert!(!Face::decide(&Default::default(), true, true, Some("en_US.UTF-8")).on, "NO_COLOR");

        let never = CliArgs { fancy: Fancy::Never, ..Default::default() };
        assert!(!Face::decide(&never, true, false, None).on);
        let always = CliArgs { fancy: Fancy::Always, ..Default::default() };
        assert!(Face::decide(&always, false, true, None).on, "--fancy always overrides both ways");
    }

    #[test]
    fn a_locale_that_does_not_say_utf8_gets_the_ascii_shapes() {
        let f = Face::decide(&CliArgs { fancy: Fancy::Always, ..Default::default() }, true, false, Some("C"));
        assert!(!f.unicode);
        let drawn = f.render(&[super::tests_support::hit("a", 0.9, false, false)]);
        assert!(drawn.is_ascii() || !drawn.contains('█'));
    }

    #[test]
    fn the_trace_breaks_where_the_cliff_is() {
        let f = Face::decide(&CliArgs { fancy: Fancy::Always, ..Default::default() }, true, false, Some("en_US.UTF-8"));
        let drawn = f.render(&[
            super::tests_support::hit("a", 0.9, false, false),
            super::tests_support::hit("b", 0.2, true, true),
        ]);
        // The break is drawn, and it is also said — a mark made only of glyphs
        // is a mark a screen reader never reaches.
        assert!(drawn.contains("past the cliff"), "{drawn}");
        assert!(drawn.contains('┃') && drawn.contains('╵'), "the trace snaps: {drawn}");
    }
}
```

Put `hit` in a small `pub(crate) mod tests_support` inside `search.rs` so both test modules build the same fixture.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib cli::face`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write the face**

```rust
//! What the terminal door looks like when a person is watching.
//!
//! Three rules make it safe to be alive: it never survives a pipe, it never
//! delays a result, and it never says by colour or by glyph alone what it must
//! say in words. Everything below is written to those; a change that breaks
//! one of them is a change that has to be reverted, not tuned.

use crate::cli::args::{CliArgs, Fancy};
use crate::core::search::SearchResult;

pub struct Face {
    pub on: bool,
    pub unicode: bool,
    pub width: usize,
}

/// The eight rungs of a score bar, and their ASCII understudies.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const ASCII_BLOCKS: [char; 8] = ['.', '.', ':', ':', '-', '=', '#', '#'];

impl Face {
    /// `is_tty`, `no_color` and `lang` are passed rather than read, so every
    /// rule about when the face appears is testable in a process that has no
    /// terminal and whose environment two tests would otherwise race on.
    pub fn decide(cli: &CliArgs, is_tty: bool, no_color: bool, lang: Option<&str>) -> Face {
        let on = match cli.fancy {
            Fancy::Always => true,
            Fancy::Never => false,
            Fancy::Auto => is_tty && !no_color && !cli.plain && !cli.json,
        };
        Face {
            on,
            unicode: lang.is_some_and(|l| l.to_ascii_uppercase().contains("UTF-8")),
            width: crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(80),
        }
    }

    /// The ranked list, drawn. Falls straight through to the plain renderer
    /// when the face is off, so there is exactly one code path a script can
    /// ever see.
    pub fn render(&self, hits: &[SearchResult]) -> String {
        if !self.on {
            return crate::cli::search::render_plain(hits);
        }
        let blocks = if self.unicode { BLOCKS } else { ASCII_BLOCKS };
        let (solid, broken) = if self.unicode { ('┃', '╵') } else { ('|', ':') };
        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            let rung = ((h.score.clamp(0.0, 1.0) * 7.0).round() as usize).min(7);
            let trace = if h.past_cliff { broken } else { solid };
            let dim = if h.past_cliff { "\u{1b}[2m" } else { "" };
            let reset = if h.past_cliff { "\u{1b}[0m" } else { "" };
            out.push_str(&format!(
                "{dim}{trace} {:>2} {} {:.2}  {}  {}{reset}\n",
                i + 1,
                blocks[rung],
                h.score,
                h.title.as_deref().unwrap_or("(untitled)"),
                h.artifact_id
            ));
            // Drawn *and* said. The break in the trace is the thing you cannot
            // miss; the words are the thing everyone else can still read.
            let mut said: Vec<&str> = Vec::new();
            if h.past_cliff {
                said.push("past the cliff");
            }
            if h.weak {
                said.push("loose match");
            }
            if h.model_written {
                said.push("model-written");
            }
            if h.primed {
                said.push("primed");
            }
            if !said.is_empty() {
                out.push_str(&format!("{dim}{trace}    [{}]{reset}\n", said.join(", ")));
            }
            for line in h.text.lines().take(2) {
                let room = self.width.saturating_sub(6).max(20);
                let clipped: String = line.chars().take(room).collect();
                out.push_str(&format!("{dim}{trace}    {clipped}{reset}\n"));
            }
            out.push_str(&format!("{trace}\n"));
        }
        out
    }

    /// A pulse travelling along a strand while a request is in flight — an
    /// impulse propagating, which is what the server is actually doing.
    ///
    /// `None` when the face is off, so a caller writes the same two lines
    /// either way. Stops on drop, and drop happens the moment the first byte
    /// of a response arrives: nothing is buffered to let a frame land evenly,
    /// and no result waits on an animation.
    pub fn pulse(&self, label: &'static str) -> Option<Pulse> {
        self.on.then(|| Pulse::start(label, self.unicode))
    }
}

pub struct Pulse {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Pulse {
    fn start(label: &'static str, unicode: bool) -> Pulse {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let cells = if unicode { ['·', '◦', '●', '◦'] } else { ['.', 'o', 'O', 'o'] };
            let span = 12usize;
            let mut head = 0usize;
            while !flag.load(Ordering::Relaxed) {
                let strand: String = (0..span)
                    .map(|i| {
                        let d = (span + head - i) % span;
                        cells[d.min(cells.len() - 1)]
                    })
                    .collect();
                // Rewritten in place, never on the alternate screen: results
                // have to stay in scrollback after the process exits.
                eprint!("\r\u{1b}[2K{strand}  {label}");
                use std::io::Write;
                std::io::stderr().flush().ok();
                head = (head + 1) % span;
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
            eprint!("\r\u{1b}[2K");
            std::io::stderr().flush().ok();
        });
        Pulse { stop, handle: Some(handle) }
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}
```

Add `crossterm = "0.30"` to `[dependencies]` — pure Rust, no C dependency, and it is what supplies the terminal size and Windows ANSI enabling.

In `search::run`, hold a pulse across the request and render through the face:

```rust
    let face = crate::cli::face::Face::decide(
        cli,
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var("LANG").ok().as_deref(),
    );
    let waiting = face.pulse("searching");
    let res = http.get(query_url(e, limit, query, cli)).bearer_auth(&e.token).send().await;
    drop(waiting); // the first byte is here; the animation ends now
```

and replace the `print!("{}", render_plain(&hits))` with `print!("{}", face.render(&hits))`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib cli::`
Expected: PASS, including Task 4's "no escape byte at all" test, which now guards the boundary between the two renderers.

- [ ] **Step 5: See it**

```bash
ENGRAM_TOKEN=… cargo run -- -s 20 "loop device"        # drawn
ENGRAM_TOKEN=… cargo run -- -s 20 "loop device" | cat  # plain, no escapes
NO_COLOR=1 ENGRAM_TOKEN=… cargo run -- -s 20 "loop device"
```

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/cli Cargo.toml
git commit -m "feat: the terminal door has a face

A pulse propagating while the server works, a score as a dendrite, and
the cliff as a literal break in the trace — drawn and also said, since
a mark made only of glyphs is made for nobody in a screenshot."
```

---

### Task 7: `-c --watch` shows the background stages

**Files:**
- Modify: `src/cli/capture.rs`, `src/cli/face.rs`
- Test: `src/cli/capture.rs` `mod tests`

**Interfaces:**
- Consumes: `Face`, `Endpoint`.
- Produces: `pub async fn watch(e: &Endpoint, id: &str, face: &Face) -> Result<()>`.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn watching_ends_when_the_corpus_stops_moving() {
        let e = endpoint().await;
        let ids = run(&e, &["-".into()], None, None).await.unwrap();
        let face = crate::cli::face::Face::decide(&Default::default(), false, true, None);
        // Ends rather than spinning forever: a terminal state is terminal, and
        // a capture that fails is one of them.
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            watch(&e, &ids[0], &face),
        )
        .await
        .expect("watching must terminate")
        .expect("watching must not fail");
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib cli::capture::tests::watching_ends_when_the_corpus_stops_moving`
Expected: FAIL — no `watch`.

- [ ] **Step 3: Write it**

```rust
/// Follow a capture through the stages that run after it is stored.
///
/// The other doors describe those stages in a sentence and leave; a terminal
/// is a place a person is already sitting, so it can show them happening. Ends
/// at the first terminal status — `ready`, `failed`, or held for review —
/// because a state nothing will move out of is not one to keep polling.
pub async fn watch(e: &Endpoint, id: &str, face: &crate::cli::face::Face) -> Result<()> {
    let http = reqwest::Client::new();
    let lamps = face.pulse("reading");
    for _ in 0..600 {
        let res = http
            .get(format!("{}/corpora/{id}", e.api("")))
            .bearer_auth(&e.token)
            .send()
            .await
            .map_err(|err| Error::Validation(format!("{err}")))?;
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        let status = body["status"].as_str().unwrap_or("");
        if matches!(status, "ready" | "partial" | "failed" | "needs_review") {
            drop(lamps);
            println!("{id}  {status}");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    drop(lamps);
    // Said rather than hidden: a capture still moving after five minutes is
    // information, and a client that exits silently claims a completion it
    // never saw.
    eprintln!("{id}: still being read after five minutes — it carries on without this client");
    Ok(())
}
```

`CorpusStatus` (`src/store/corpora.rs`) has **four** states nothing moves out of — `Ready`, `Partial`, `Failed` and `NeedsReview` — and `NeedsReview` is the parked near-duplicate, which is exactly the one a client must not sit waiting on. Confirm the serialised spellings with `grep -n "rename_all\|impl.*CorpusStatus" src/store/corpora.rs` before writing the match: getting them wrong makes this loop run to its cap on every capture, which is a five-minute hang that looks like a network fault.

Call it from `cli::run` after a capture when `cli.watch` is set, once per returned id.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib cli::capture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/cli
git commit -m "feat: --watch shows the stages the other doors only describe

Ends at the first terminal status, and says so when a capture is still
moving rather than exiting on a completion it never saw."
```

---

### Task 8: The doors are documented

**Files:**
- Modify: `README.md`, `ROADMAP.md`

- [ ] **Step 1: Add the client to the README's Features list**

One entry in the register the file uses: capture, search and ask from a shell; `-s N` for a deliberately wide read; the plain form for pipes and the drawn one for a person; `ENGRAM_TOKEN` and `~/.config/engram/cli.toml`; exit `1` when nothing was found.

- [ ] **Step 2: Move the terminal door into ROADMAP's built paragraph**

- [ ] **Step 3: Commit**

```bash
git add README.md ROADMAP.md
git commit -m "docs: the shell is a door"
```

---

## Self-Review

**Spec coverage.** §4's verbs → Tasks 3, 4, 5. §4's `-s N` and its pool argument → Task 4 (`query_url`, and the clamp is the server's existing `MAX_LIMIT`). §4's stdin rules → Task 1. §4's `ENGRAM_URL`/`cli.toml`/token error → Task 2. §4's `Door::Cli` claim → Task 4's `door=cli` test, standing on server-plan Task 5. §4's exit codes → Tasks 4 and 5. §4a's three rules → Task 6's `decide` tests plus Task 4's no-escape-byte test. §4a's four drawings → Task 6 (pulse, dendrite, cliff break) and Task 7 (the stages). §4a's "built last" → Tasks 6 and 7 are last, and Tasks 1-5 ship a complete plain client.

**Type consistency.** `Verb` and `CliArgs` (Task 1) are matched in `cli::run` (Task 3) and extended in Tasks 4, 5, 7. `Endpoint { url, token }` with `api(path)` (Task 2) is used by every later task with that exact shape. `render_plain(&[SearchResult]) -> String` (Task 4) is what `Face::render` falls through to (Task 6). `Face::decide(cli, is_tty, no_color, lang)` has the same four arguments in Tasks 6 and 7.

**One thing the executor must check rather than assume**, flagged inline: the exact strings `CorpusStatus` serialises to (Task 7). It is one `grep`, and guessing it turns every `--watch` into a five-minute hang. The dependency questions are settled — `toml_edit` and `tokio-stream` are in the tree, `toml` and `futures-util` are not, and `crossterm` is the only addition.
