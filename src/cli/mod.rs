//! The terminal door: capture, search and ask over HTTP, never into the store, so a shell search
//! is a real recorded search behind the same ranking and tenancy checks.

pub mod args;
pub mod ask;
pub mod capture;
pub mod decide;
pub mod endpoint;
pub mod face;
pub mod last;
pub mod search;
pub mod show;
pub mod status;
#[cfg(test)]
pub(crate) mod test_support;

/// Percent-encode a query value: only what would end the value or the query.
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

/// An error as a sentence for the terminal, without `Validation`'s kind prefix.
pub fn say(e: &crate::error::Error) -> String {
    match e {
        crate::error::Error::Validation(msg) => msg.clone(),
        other => other.to_string(),
    }
}

/// Run a verb and return the exit code: `0` results, `1` none, `2` a failure. Shells branch on it.
pub async fn run(verb: args::Verb, cli: &args::CliArgs) -> i32 {
    let endpoint = match endpoint::resolve(
        &|k| std::env::var(k).ok(),
        endpoint::default_path().as_deref(),
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", say(&e));
            return 2;
        }
    };
    dispatch(&endpoint, verb, cli).await
}

/// `run` past the endpoint, so a test can name the server.
pub(crate) async fn dispatch(
    endpoint: &endpoint::Endpoint,
    verb: args::Verb,
    cli: &args::CliArgs,
) -> i32 {
    // One face for the whole invocation, so the upload track, readout and lamps agree.
    let face = face::Face::decide(
        cli,
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
        std::env::var_os("NO_COLOR").is_some(),
        crate::cli::face::locale().as_deref(),
    );
    match verb {
        args::Verb::Capture(_) | args::Verb::CapturePiped(_) => {
            let captured = match &verb {
                args::Verb::Capture(targets) => {
                    capture::run(
                        endpoint,
                        targets,
                        cli.title.as_deref(),
                        cli.note.as_deref(),
                        &face,
                    )
                    .await
                }
                args::Verb::CapturePiped(text) => {
                    capture::run_piped(
                        endpoint,
                        text.clone(),
                        cli.title.as_deref(),
                        cli.note.as_deref(),
                        &face,
                    )
                    .await
                }
                _ => unreachable!("the arm this match is inside"),
            };
            match captured {
                Ok(ids) => {
                    for id in &ids {
                        println!("{id}");
                    }
                    if cli.watch {
                        for id in &ids {
                            if let Err(e) = capture::watch(endpoint, id, &face).await {
                                eprintln!("{}", say(&e));
                                return 2;
                            }
                        }
                    }
                    0
                }
                Err(e) => {
                    eprintln!("{}", say(&e));
                    2
                }
            }
        }
        args::Verb::Search { limit, query } => {
            match search::run(endpoint, limit, &query, cli).await {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("{}", say(&e));
                    2
                }
            }
        }
        args::Verb::Show(which) => match show::run(endpoint, &which, last::load()).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{}", say(&e));
                2
            }
        },
        args::Verb::Status => match status::run(endpoint, &face, cli.json).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{}", say(&e));
                2
            }
        },
        args::Verb::Ask(question) => match ask::run(endpoint, &question, cli).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{}", say(&e));
                2
            }
        },
        args::Verb::Decide {
            question,
            kind,
            situation,
        } => match decide::run(endpoint, &question, &kind, situation.as_deref(), cli).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{}", say(&e));
                2
            }
        },
        args::Verb::Remind(text) => {
            match capture::run_text(
                endpoint,
                text,
                cli.title.as_deref(),
                cli.note.as_deref(),
                Some("remind"),
                &face,
            )
            .await
            {
                Ok(id) => {
                    println!("{id}");
                    0
                }
                Err(e) => {
                    eprintln!("{}", say(&e));
                    2
                }
            }
        }
    }
}
