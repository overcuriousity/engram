//! The terminal door: capture, search and ask from a shell.
//!
//! Over HTTP, never into the store — one set of ranking parameters, tenancy
//! checks and feedback recording stands behind every door, and a search typed
//! at a shell is a real recorded search the judge page can grade later.

pub mod args;
pub mod ask;
pub mod capture;
pub mod endpoint;
pub mod face;
pub mod search;
#[cfg(test)]
pub(crate) mod test_support;

/// Percent-encode a query value.
///
/// Only what would end the value or the query; nothing here is a
/// general-purpose encoder, and it is one function rather than one per verb so
/// two doors cannot disagree about what needs escaping.
pub(crate) fn encode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Run a verb and answer with the process's exit code: `0` results, `1` none,
/// `2` a failure.
///
/// A shell branches on these — `engram -s "x" || …` — so they are part of the
/// interface rather than a detail, and every verb answers in the same three.
pub async fn run(verb: args::Verb, cli: &args::CliArgs) -> i32 {
    let endpoint = match endpoint::resolve(
        &|k| std::env::var(k).ok(),
        endpoint::default_path().as_deref(),
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    match verb {
        args::Verb::Capture(targets) => {
            match capture::run(
                &endpoint,
                &targets,
                cli.title.as_deref(),
                cli.note.as_deref(),
            )
            .await
            {
                Ok(ids) => {
                    // One id per line, which is what a shell wants to pipe into
                    // whatever it does next.
                    for id in &ids {
                        println!("{id}");
                    }
                    if cli.watch {
                        let face = face::Face::decide(
                            cli,
                            std::io::IsTerminal::is_terminal(&std::io::stdout()),
                            std::env::var_os("NO_COLOR").is_some(),
                            std::env::var("LANG").ok().as_deref(),
                        );
                        for id in &ids {
                            if let Err(e) = capture::watch(&endpoint, id, &face).await {
                                eprintln!("{e}");
                                return 2;
                            }
                        }
                    }
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    2
                }
            }
        }
        args::Verb::Search { limit, query } => {
            match search::run(&endpoint, limit, &query, cli).await {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("{e}");
                    2
                }
            }
        }
        args::Verb::Ask(question) => match ask::run(&endpoint, &question, cli).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{e}");
                2
            }
        },
    }
}
