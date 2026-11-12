//! Which verb an invocation asked for: a pure function of the flags and stdin, so it is testable.

use crate::error::{Error, Result};

/// The client half of the binary's arguments, flattened into `Args` in `main.rs`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct CliArgs {
    /// Capture a file, a link, a sentence, or `-` for standard input. Repeatable.
    #[arg(short = 'c', value_name = "PATH|URL|TEXT|-", num_args = 1.., conflicts_with_all = ["search", "ask", "remind"])]
    pub capture: Vec<String>,
    /// Search. A leading bare integer is how many hits are wanted.
    #[arg(short = 's', value_name = "[N] QUERY", num_args = 1.., conflicts_with_all = ["capture", "ask", "remind"])]
    pub search: Vec<String>,
    /// Ask one question across the base.
    #[arg(short = 'a', value_name = "QUESTION", num_args = 1.., conflicts_with_all = ["capture", "search", "remind"])]
    pub ask: Vec<String>,
    /// Capture as a reminder: the classifier is skipped and the note is dated.
    #[arg(short = 'r', value_name = "TEXT", num_args = 1.., conflicts_with_all = ["capture", "search", "ask", "show", "status"])]
    pub remind: Vec<String>,
    /// What to call this capture (only with `-c`).
    #[arg(long, value_name = "TITLE", conflicts_with_all = ["search", "ask", "show", "status"])]
    pub title: Option<String>,
    /// A note stored beside the capture: where it came from, why it matters.
    #[arg(long, value_name = "NOTE", conflicts_with_all = ["search", "ask", "show", "status"])]
    pub note: Option<String>,
    /// Narrow a search to artifacts carrying this tag. Repeatable.
    #[arg(long = "tag", value_name = "TAG", conflicts_with_all = ["ask", "show", "status", "remind"])]
    pub tags: Vec<String>,
    /// Narrow a search to artifacts in this category.
    #[arg(
        long,
        value_name = "CATEGORY",
        conflicts_with_all = ["ask", "show", "status", "remind"]
    )]
    pub category: Option<String>,
    /// Print the results as JSON instead of for a person.
    #[arg(long, conflicts_with_all = ["ask", "show", "remind"])]
    pub json: bool,
    /// Never colour, never animate, never leave ASCII.
    #[arg(long)]
    pub plain: bool,
    /// Override the terminal detection in either direction.
    #[arg(long, value_name = "WHEN", default_value = "auto")]
    pub fancy: Fancy,
    /// After capturing, follow the background stages until they finish.
    #[arg(long, conflicts_with_all = ["search", "ask", "show", "status", "remind"])]
    pub watch: bool,
    /// Read one artifact in full: a rank from the last search, an id prefix, or a whole id.
    #[arg(long, value_name = "RANK|ID", conflicts_with_all = ["capture", "search", "ask"])]
    pub show: Option<String>,
    /// What the base holds, what it is working through, and what it has been learning.
    #[arg(long, conflicts_with_all = ["capture", "search", "ask", "show"])]
    pub status: bool,
    // Flags `--show` does not honour refuse it from their side, so each conflict is declared once;
    // `--status` likewise, except `--json`, which it honours.
}

/// When the drawn rendering is used; `Auto` tells a pipe from a terminal.
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
    /// What arrived on stdin with no verb flag, already read: stdin does not rewind.
    CapturePiped(String),
    Search {
        limit: Option<usize>,
        query: String,
    },
    Ask(String),
    Remind(String),
    Status,
    /// One artifact, read in full; carries what was typed, as a rank needs the remembered list.
    Show(String),
}

/// The verb, or `None` to be the server. `stdin` is a closure so this runs in a test.
/// `other_command`: a server-side command was named, which `CliArgs` cannot see.
pub fn verb(
    args: &CliArgs,
    other_command: bool,
    stdin_piped: bool,
    stdin: impl FnOnce() -> std::io::Result<String>,
) -> Result<Option<Verb>> {
    let named = [
        !args.capture.is_empty(),
        !args.search.is_empty(),
        !args.ask.is_empty(),
        !args.remind.is_empty(),
        args.show.is_some(),
        args.status,
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if other_command {
        // Before stdin is read: `echo yes | engram --delete-user x` must not capture "yes".
        if named > 0 {
            return Err(Error::Validation(
                "`-c`, `-s`, `-a`, `-r`, `--show` and `--status` are the client half \
                 of this binary; run them on their own, without the server's \
                 own flags"
                    .into(),
            ));
        }
        return Ok(None);
    }

    if named > 1 {
        // clap refuses this first; this is for callers that build `CliArgs` themselves.
        return Err(Error::Validation(
            "one verb at a time: `-c`, `-s`, `-a`, `-r`, `--show` or `--status`".into(),
        ));
    }

    // Before the pipe: `engram --status | grep failed` is not a capture.
    if args.status {
        return Ok(Some(Verb::Status));
    }
    if let Some(which) = &args.show {
        // Before the pipe: `engram --show 3 | less` is not a capture.
        return Ok(Some(Verb::Show(which.clone())));
    }
    if !args.capture.is_empty() {
        // Not read here: the verb decides what a path or link means.
        return Ok(Some(Verb::Capture(args.capture.clone())));
    }
    if !args.ask.is_empty() {
        return Ok(Some(Verb::Ask(read(&args.ask, stdin)?)));
    }
    if !args.remind.is_empty() {
        return Ok(Some(Verb::Remind(read(&args.remind, stdin)?)));
    }
    if !args.search.is_empty() {
        // A leading integer is a count only when something is left to be the query.
        let (limit, rest) = match args.search.split_first() {
            Some((head, rest)) if !rest.is_empty() => match head.parse::<usize>() {
                Ok(n) => (Some(n), rest.to_vec()),
                Err(_) => (None, args.search.clone()),
            },
            _ => (None, args.search.clone()),
        };
        return Ok(Some(Verb::Search {
            limit,
            query: read(&rest, stdin)?,
        }));
    }

    // No verb: a pipe is a capture. Decided on what was read: systemd, `nohup` and cron hand a
    // service `/dev/null`, and an empty stdin keeps `engram` the server.
    if stdin_piped {
        let piped = stdin().map_err(|e| Error::Validation(format!("stdin: {e}")))?;
        if piped.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(Verb::CapturePiped(piped)));
    }
    Ok(None)
}

/// The words a verb was given, or stdin when they are just `-`.
fn read(words: &[String], stdin: impl FnOnce() -> std::io::Result<String>) -> Result<String> {
    let joined = words.join(" ");
    if joined.trim() == "-" {
        return stdin()
            .map(|s| s.trim().to_string())
            .map_err(|e| Error::Validation(format!("stdin: {e}")));
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {

    #[test]
    fn status_is_a_verb_of_its_own() {
        let a = CliArgs {
            status: true,
            ..Default::default()
        };
        assert_eq!(
            verb(&a, false, false, || Ok(String::new())).unwrap(),
            Some(Verb::Status)
        );
    }

    #[test]
    fn status_down_a_pipe_is_still_status_and_not_a_capture() {
        let a = CliArgs {
            status: true,
            ..Default::default()
        };
        assert_eq!(
            verb(&a, false, true, || Ok("some piped text".into())).unwrap(),
            Some(Verb::Status)
        );
    }

    #[test]
    fn status_does_not_share_an_invocation_with_another_verb() {
        let a = CliArgs {
            status: true,
            search: vec!["x".into()],
            ..Default::default()
        };
        assert!(verb(&a, false, false, || Ok(String::new())).is_err());
    }
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
        let v = verb(&a, false, false, piped("")).unwrap().unwrap();
        assert!(
            matches!(v, Verb::Search { limit: Some(40), ref query } if query == "qdrant payload filter")
        );
    }

    #[test]
    fn a_query_that_is_itself_a_number_is_reachable() {
        // After `--`, clap stops reading flags: it is a query, not a count.
        let mut a = args();
        a.search = vec!["42".into()];
        let v = verb(&a, false, false, piped("")).unwrap().unwrap();
        assert!(
            matches!(v, Verb::Search { limit: None, ref query } if query == "42"),
            "a lone number is the query — there is nothing left for it to count"
        );
    }

    #[test]
    fn stdin_is_the_value_of_whichever_verb_was_named() {
        let mut a = args();
        a.search = vec!["-".into()];
        let v = verb(&a, false, true, piped("loop device"))
            .unwrap()
            .unwrap();
        assert!(matches!(v, Verb::Search { ref query, .. } if query == "loop device"));
    }

    #[test]
    fn a_pipe_with_no_verb_at_all_is_a_capture() {
        let v = verb(&args(), false, true, piped("a procedure worth keeping"))
            .unwrap()
            .unwrap();
        assert!(matches!(v, Verb::CapturePiped(ref t) if t == "a procedure worth keeping"));
    }

    #[test]
    fn a_service_handed_dev_null_is_still_the_server() {
        // systemd, `nohup`, `cron` and `engram &` hand stdin `/dev/null`; none asks for a capture.
        assert!(
            verb(&args(), false, true, piped("")).unwrap().is_none(),
            "an empty stdin is not an instruction"
        );
        assert!(
            verb(&args(), false, true, piped("  \n "))
                .unwrap()
                .is_none(),
            "and neither is whitespace"
        );
    }

    #[test]
    fn a_terminal_with_no_verb_is_the_server_it_has_always_been() {
        assert!(
            verb(&args(), false, false, piped("")).unwrap().is_none(),
            "`engram` alone must not change meaning"
        );
    }

    #[test]
    fn status_refuses_every_flag_it_does_not_honour() {
        #[derive(clap::Parser)]
        struct OnlyTheClientHalf {
            #[command(flatten)]
            cli: CliArgs,
        }
        let parsed = |argv: &[&str]| {
            <OnlyTheClientHalf as clap::Parser>::try_parse_from(
                std::iter::once("engram").chain(argv.iter().copied()),
            )
        };
        for dropped in [
            vec!["--watch"],
            vec!["--tag", "linux"],
            vec!["--category", "forensik"],
            vec!["--title", "x"],
            vec!["--note", "x"],
        ] {
            let mut argv = vec!["--status"];
            argv.extend_from_slice(&dropped);
            assert!(
                parsed(&argv).is_err(),
                "accepted and then dropped: engram {}",
                argv.join(" ")
            );
        }
        assert!(parsed(&["--status", "--json"]).unwrap().cli.status);
    }

    #[test]
    fn two_verbs_in_one_invocation_are_refused() {
        let mut a = args();
        a.search = vec!["a".into()];
        a.ask = vec!["b".into()];
        assert!(verb(&a, false, false, piped("")).is_err());
    }

    #[test]
    fn show_names_one_artifact_to_read_in_full() {
        let a = CliArgs {
            show: Some("3".into()),
            ..Default::default()
        };
        let v = verb(&a, false, false, piped("")).unwrap().unwrap();
        assert!(matches!(v, Verb::Show(ref which) if which == "3"));
    }

    #[test]
    fn show_alongside_another_verb_is_refused() {
        let a = CliArgs {
            show: Some("3".into()),
            search: vec!["forensik".into()],
            ..Default::default()
        };
        assert!(verb(&a, false, false, piped("")).is_err());
    }

    #[test]
    fn show_down_a_pipe_is_still_a_reading_and_not_a_capture() {
        let a = CliArgs {
            show: Some("3".into()),
            ..Default::default()
        };
        let v = verb(&a, false, true, piped("something")).unwrap().unwrap();
        assert!(matches!(v, Verb::Show(ref which) if which == "3"));
    }

    #[test]
    fn a_piped_answer_to_a_prompt_is_not_a_capture() {
        assert!(
            verb(&args(), true, true, piped("yes")).unwrap().is_none(),
            "a pipe alongside a server command is an answer, not a note"
        );
    }

    #[test]
    fn stdin_is_left_alone_when_the_server_was_asked_for() {
        let read = std::cell::Cell::new(false);
        let v = verb(&args(), true, true, || {
            read.set(true);
            Ok("yes".into())
        })
        .unwrap();
        assert!(v.is_none());
        assert!(!read.get(), "stdin was drained out from under the prompt");
    }

    #[test]
    fn a_verb_flag_alongside_a_server_command_is_refused() {
        let a = CliArgs {
            capture: vec!["notes.pdf".into()],
            ..Default::default()
        };
        assert!(verb(&a, true, false, piped("")).is_err());
    }

    #[test]
    fn a_reminder_is_a_verb_of_its_own() {
        let r = CliArgs {
            remind: vec!["call".into(), "the".into(), "bank".into()],
            ..Default::default()
        };
        assert!(
            matches!(verb(&r, false, false, || Ok(String::new())), Ok(Some(Verb::Remind(t))) if t == "call the bank")
        );
        let both = CliArgs {
            remind: vec!["x".into()],
            capture: vec!["y".into()],
            ..Default::default()
        };
        assert!(
            verb(&both, false, false, || Ok(String::new())).is_err(),
            "one verb at a time"
        );
    }
}
