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
    /// What to call this capture. Refused with every verb but `-c`, for the
    /// reason `--tag` is refused with `-a`: a search has no title to set.
    #[arg(long, value_name = "TITLE", conflicts_with_all = ["search", "ask", "show"])]
    pub title: Option<String>,
    #[arg(long, value_name = "NOTE", conflicts_with_all = ["search", "ask", "show"])]
    pub note: Option<String>,
    /// Narrow a search to artifacts carrying this tag. Repeatable.
    ///
    /// Refused with `-a`, along with `--category` and `--json`: the ask door
    /// accepts all three over its JSON API, but no interactive door offers a
    /// filtered ask, and a flag that is accepted and then dropped is worse
    /// than one that is refused. See `cli::ask::run`.
    ///
    /// Refused with `--show` for the plainer version of the same reason: one
    /// artifact named by id is not a list there is anything to narrow.
    #[arg(long = "tag", value_name = "TAG", conflicts_with_all = ["ask", "show"])]
    pub tags: Vec<String>,
    #[arg(long, value_name = "CATEGORY", conflicts_with_all = ["ask", "show"])]
    pub category: Option<String>,
    /// Print the results as JSON instead of for a person.
    ///
    /// `--show` refuses it rather than ignoring it: the reading door renders a
    /// body for a person to read and has no JSON form, and being handed the
    /// human rendering after asking for JSON is the failure this whole rule is
    /// about.
    #[arg(long, conflicts_with_all = ["ask", "show"])]
    pub json: bool,
    /// Never colour, never animate, never leave ASCII.
    #[arg(long)]
    pub plain: bool,
    /// Override the terminal detection in either direction.
    #[arg(long, value_name = "WHEN", default_value = "auto")]
    pub fancy: Fancy,
    /// After capturing, follow the background stages until they finish.
    ///
    /// There is nothing to follow behind the other three verbs: they finish
    /// when their response arrives.
    #[arg(long, conflicts_with_all = ["search", "ask", "show"])]
    pub watch: bool,
    /// Read one artifact in full: a rank from the last search, a leading piece
    /// of an id, or a whole id.
    #[arg(long, value_name = "RANK|ID", conflicts_with_all = ["capture", "search", "ask"])]
    pub show: Option<String>,
    /// What the base holds, what it is working through, and what it has been
    /// learning.
    ///
    /// One shot, like every other verb. There is no ambient status line and no
    /// footer under an ordinary search: that would make the cheapest door in
    /// the application pay a request for something nobody asked for at that
    /// moment.
    #[arg(long, conflicts_with_all = ["capture", "search", "ask", "show"])]
    pub status: bool,
    // Every flag `--show` does not honour refuses it from the other side —
    // `--json`, `--tag`, `--category`, `--title`, `--note`, `--watch` — so the
    // conflict is declared once, on the flag whose own doc comment explains it.
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
    /// What arrived on stdin with no verb flag, already read.
    ///
    /// Carried rather than re-read: deciding whether a pipe held anything
    /// consumes it, and stdin does not rewind.
    CapturePiped(String),
    Search {
        limit: Option<usize>,
        query: String,
    },
    Ask(String),
    /// What the base holds and what it has been learning.
    Status,
    /// One artifact, read in full. Carries what the operator typed rather than
    /// an id: a rank means nothing until the remembered list is read, and that
    /// is a file, which is not something this decision touches.
    Show(String),
}

/// The verb, or `None` for "no verb was asked for — be the server".
///
/// `stdin_piped` is passed rather than detected, and `stdin` is a closure
/// rather than a read, so the whole decision runs in a test with no terminal
/// and no process to pipe into.
///
/// `other_command` says the invocation already named something this binary
/// does as the server — `--delete-user`, `--reindex`, `--print-config`. Asked
/// for rather than derived, because `CliArgs` is the client half and cannot
/// see the other half's flags.
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
        args.show.is_some(),
        args.status,
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if other_command {
        // Answered before stdin is touched, which is the whole of why this
        // parameter exists. `--delete-user` prints a prompt and reads the
        // answer, and the obvious way to script past it is
        // `echo yes | engram --delete-user sub@idp`. Without this, the pipe is
        // taken for the gesture the terminal door exists for: "yes" is stored
        // as a corpus, the process exits 0, and nothing is deleted. Reading
        // stdin to find that out would be just as wrong the other way — the
        // prompt below would then be handed EOF.
        if named > 0 {
            return Err(Error::Validation(
                "`-c`, `-s`, `-a`, `--show` and `--status` are the client half \
                 of this binary; run them on their own, without the server's \
                 own flags"
                    .into(),
            ));
        }
        return Ok(None);
    }

    if named > 1 {
        // clap's `conflicts_with_all` refuses this before we are reached, and
        // this is here for the caller that built `CliArgs` itself — which is
        // every test, and one day some other entry point.
        return Err(Error::Validation(
            "one verb at a time: `-c`, `-s`, `-a`, `--show` or `--status`".into(),
        ));
    }

    // Answered before the pipe is looked at, for the reason `--show` is:
    // `engram --status | grep failed` is an ordinary thing to type, and without
    // this the pipe would be taken for the capture gesture below.
    if args.status {
        return Ok(Some(Verb::Status));
    }
    if let Some(which) = &args.show {
        // Answered before the pipe is looked at. `engram --show 3 | less` is
        // the ordinary way to read a long artifact, and without this the pipe
        // would be taken for the capture gesture the door below exists for.
        return Ok(Some(Verb::Show(which.clone())));
    }
    if !args.capture.is_empty() {
        // Not read here: a capture target may be a path or a link, and the
        // reading of each belongs to the verb that knows what to do with it.
        return Ok(Some(Verb::Capture(args.capture.clone())));
    }
    if !args.ask.is_empty() {
        return Ok(Some(Verb::Ask(read(&args.ask, stdin)?)));
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
            query: read(&rest, stdin)?,
        }));
    }

    // No verb named. A pipe is still an instruction: capturing what was piped
    // is the gesture the whole terminal door exists for, and requiring `-c -`
    // there would be ceremony in front of the one case that has to be
    // frictionless.
    //
    // But "not a terminal" is not "a pipe". A service started by systemd is
    // handed `/dev/null` on stdin, and so is anything under `nohup`, `cron` or
    // a bare `engram &` — none of which is a terminal and none of which is
    // asking for a capture. Reading first and deciding on what came back is
    // what keeps `engram` the server in every one of those: an empty stdin is
    // not an instruction, so the binary stays what it has always been.
    if stdin_piped {
        let piped = stdin().map_err(|e| Error::Validation(format!("stdin: {e}")))?;
        if piped.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(Verb::CapturePiped(piped)));
    }
    Ok(None)
}

/// The words a verb was given, or what stdin held when they are just `-`.
///
/// A free function rather than a closure inside `verb`, because the closure
/// would have to capture the one-shot reader and every branch wants it: the
/// borrow checker is right that only one of them may have it, and each branch
/// returns, so handing it over per branch is the shape that says so.
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

    /// Answered before the pipe is looked at, exactly as `--show` is:
    /// `engram --status | grep failed` is an ordinary thing to type.
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
        // `-s -- 42` puts the number after the separator, where clap stops
        // reading flags: it is a query, not a count.
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
        // systemd gives a unit `/dev/null` on stdin unless told otherwise, and
        // so do `nohup`, `cron` and `engram &`. None of them is a terminal and
        // none of them is asking for a capture: reading a service's stdin as
        // an instruction is a server that never starts.
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

    /// A reading is its own verb, so it is refused alongside another one for
    /// the same reason `-s` and `-a` are: quietly picking one loses the other.
    #[test]
    fn show_alongside_another_verb_is_refused() {
        let a = CliArgs {
            show: Some("3".into()),
            search: vec!["forensik".into()],
            ..Default::default()
        };
        assert!(verb(&a, false, false, piped("")).is_err());
    }

    /// And `engram --show 3 | less` must stay a reading. Without `show` in the
    /// count, the pipe would be read as the capture gesture and the id would
    /// be stored as a note.
    #[test]
    fn show_down_a_pipe_is_still_a_reading_and_not_a_capture() {
        let a = CliArgs {
            show: Some("3".into()),
            ..Default::default()
        };
        let v = verb(&a, false, true, piped("something")).unwrap().unwrap();
        assert!(matches!(v, Verb::Show(ref which) if which == "3"));
    }

    /// The gesture this guards: `--delete-user` prints a prompt, and the
    /// obvious way to script past it is to pipe the answer in. Without the
    /// guard the pipe was read as a capture, "yes" became a corpus, the
    /// process exited 0 and the account was still there.
    #[test]
    fn a_piped_answer_to_a_prompt_is_not_a_capture() {
        assert!(
            verb(&args(), true, true, piped("yes")).unwrap().is_none(),
            "a pipe alongside a server command is an answer, not a note"
        );
    }

    /// And stdin is not touched on the way to finding that out, or the prompt
    /// the pipe was answering would be handed EOF.
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

    /// A verb flag *and* a server command is neither one thing nor the other,
    /// and quietly picking the server would lose the capture in silence.
    #[test]
    fn a_verb_flag_alongside_a_server_command_is_refused() {
        let a = CliArgs {
            capture: vec!["notes.pdf".into()],
            ..Default::default()
        };
        assert!(verb(&a, true, false, piped("")).is_err());
    }
}
