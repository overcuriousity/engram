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
    /// Override the terminal detection in either direction.
    #[arg(long, value_name = "WHEN", default_value = "auto")]
    pub fancy: Fancy,
    /// After capturing, follow the background stages until they finish.
    #[arg(long)]
    pub watch: bool,
}

/// When the drawn rendering is used. `Auto` is the only honest default: a pipe
/// and a terminal want different things and the process can tell which it has.
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
    let named = [
        !args.capture.is_empty(),
        !args.search.is_empty(),
        !args.ask.is_empty(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if named > 1 {
        // clap's `conflicts_with_all` refuses this before we are reached, and
        // this is here for the caller that built `CliArgs` itself — which is
        // every test, and one day some other entry point.
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
        // Not read here: a capture target may be a path or a link, and the
        // reading of each belongs to the verb that knows what to do with it.
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
        return Ok(Some(Verb::Search {
            limit,
            query: read(&rest)?,
        }));
    }

    // No verb named. A pipe is still an instruction: capturing what was piped
    // is the gesture the whole terminal door exists for, and requiring `-c -`
    // there would be ceremony in front of the one case that has to be
    // frictionless. A terminal on stdin is no instruction at all, so the
    // binary stays the server it has always been.
    if stdin_piped {
        return Ok(Some(Verb::Capture(vec!["-".into()])));
    }
    Ok(None)
}

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
        assert!(
            matches!(v, Verb::Search { limit: Some(40), ref query } if query == "qdrant payload filter")
        );
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
        let v = verb(&args(), true, piped("a procedure worth keeping"))
            .unwrap()
            .unwrap();
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
