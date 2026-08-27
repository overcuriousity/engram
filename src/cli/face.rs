//! What the terminal door looks like when a person is watching.
//!
//! Three rules make it safe to be alive: it never survives a pipe, it never
//! delays a result, and it never says by colour or by glyph alone what it must
//! say in words. Everything below is written to those; a change that breaks one
//! of them is a change to revert rather than to tune.

use crate::cli::args::{CliArgs, Fancy};
use crate::core::search::SearchResult;

pub struct Face {
    pub on: bool,
    pub unicode: bool,
    pub width: usize,
}

/// The eight rungs of a score bar, and their ASCII understudies. The same eight
/// steps either way, so the shape of a list does not change with the locale.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const ASCII_BLOCKS: [char; 8] = ['.', '.', ':', ':', '-', '=', '#', '#'];

/// Dim, and back. Written out rather than pulled from a styling crate: two
/// escapes used in three places do not need a dependency's opinion.
const DIM: &str = "\u{1b}[2m";
const RESET: &str = "\u{1b}[0m";

/// The locale that decides whether the glyphs are drawable, in POSIX order.
///
/// `LC_ALL` overrides everything; `LC_CTYPE` is the category that actually
/// governs character handling; `LANG` is the fallback. Reading `LANG` alone
/// was wrong in both directions: macOS commonly ships `LC_CTYPE=UTF-8` with
/// no `LANG` at all, and those terminals were handed the ASCII understudies
/// though they render `▇` and `┃` perfectly — while `LC_ALL=C` alongside a
/// UTF-8 `LANG` got box-drawing it cannot show.
///
/// An empty value is unset, which is what the shell means by `LC_ALL=`.
///
/// `get` is passed rather than read, for the reason `decide` takes its facts
/// as arguments: two tests must not race on one process's environment.
pub fn locale_from(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .filter_map(get)
        .find(|v| !v.is_empty())
}

/// `locale_from`, against this process.
pub fn locale() -> Option<String> {
    locale_from(|k| std::env::var(k).ok())
}

impl Face {
    /// `is_tty`, `no_color` and `lang` are passed rather than read, so every
    /// rule about when the face appears is testable in a process that has no
    /// terminal and whose environment two tests would otherwise race on.
    /// `lang` is the whole locale, resolved by `locale` — not `LANG`.
    pub fn decide(cli: &CliArgs, is_tty: bool, no_color: bool, lang: Option<&str>) -> Face {
        let on = match cli.fancy {
            Fancy::Always => true,
            Fancy::Never => false,
            // `--json` is off as well: its reader is a machine even when a
            // person is watching it arrive.
            Fancy::Auto => is_tty && !no_color && !cli.plain && !cli.json,
        };
        Face {
            on,
            unicode: lang.is_some_and(|l| l.to_ascii_uppercase().contains("UTF-8")),
            width: crossterm::terminal::size()
                .map(|(w, _)| w as usize)
                .unwrap_or(80),
        }
    }

    /// The ranked list, drawn.
    ///
    /// Falls straight through to the plain renderer when the face is off, so
    /// there is exactly one code path a script can ever see.
    pub fn render(&self, hits: &[SearchResult]) -> String {
        if !self.on {
            return crate::cli::search::render_plain(hits);
        }
        let blocks = if self.unicode { BLOCKS } else { ASCII_BLOCKS };
        // The trace running down the list, and what it becomes past the cliff.
        let (solid, broken) = if self.unicode {
            ('┃', '╵')
        } else {
            ('|', ':')
        };
        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            let rung = ((h.score.clamp(0.0, 1.0) * 7.0).round() as usize).min(7);
            let trace = if h.past_cliff { broken } else { solid };
            let (dim, reset) = if h.past_cliff { (DIM, RESET) } else { ("", "") };
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
            if let Some(said) = crate::cli::search::badges(h) {
                out.push_str(&format!("{dim}{trace}    [{said}]{reset}\n"));
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
    /// either way. It stops on drop, and drop happens the moment the response
    /// arrives: nothing is buffered to let a frame land evenly, and no result
    /// waits on an animation.
    ///
    /// Drawn on stderr, so a redirected stdout never receives a frame of it
    /// even in the case where someone forced `--fancy always` into a pipe.
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
            // One bright cell with a decaying tail behind it.
            let cells = if unicode {
                ['●', '◦', '·', '·']
            } else {
                ['O', 'o', '.', '.']
            };
            let span = 12usize;
            let mut head = 0usize;
            while !flag.load(Ordering::Relaxed) {
                let strand: String = (0..span)
                    .map(|i| {
                        let behind = (span + head - i) % span;
                        cells[behind.min(cells.len() - 1)]
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
            use std::io::Write;
            std::io::stderr().flush().ok();
        });
        Pulse {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // Joined rather than detached: the line has to be erased before
            // anything else prints, or a result lands on top of a half-drawn
            // strand.
            h.join().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{CliArgs, Fancy};
    use crate::cli::search::fixture::hit;

    fn always() -> CliArgs {
        CliArgs {
            fancy: Fancy::Always,
            ..Default::default()
        }
    }

    #[test]
    fn the_face_is_off_wherever_it_could_reach_a_machine() {
        let plain = CliArgs {
            plain: true,
            ..Default::default()
        };
        assert!(
            !Face::decide(&plain, true, false, Some("en_US.UTF-8")).on,
            "--plain wins over a terminal"
        );
        assert!(
            !Face::decide(&Default::default(), false, false, Some("en_US.UTF-8")).on,
            "a pipe"
        );
        assert!(
            !Face::decide(&Default::default(), true, true, Some("en_US.UTF-8")).on,
            "NO_COLOR"
        );
        let json = CliArgs {
            json: true,
            ..Default::default()
        };
        assert!(
            !Face::decide(&json, true, false, Some("en_US.UTF-8")).on,
            "--json is read by a machine even on a terminal"
        );

        let never = CliArgs {
            fancy: Fancy::Never,
            ..Default::default()
        };
        assert!(!Face::decide(&never, true, false, None).on);
        assert!(
            Face::decide(&always(), false, true, None).on,
            "--fancy always overrides both ways"
        );
    }

    #[test]
    fn a_locale_that_does_not_say_utf8_gets_the_ascii_shapes() {
        let f = Face::decide(&always(), true, false, Some("C"));
        assert!(!f.unicode);
        let drawn = f.render(&[hit("a", 0.9, false, false)]);
        assert!(
            !drawn.contains('█') && !drawn.contains('┃'),
            "a drawing glyph reached a non-UTF-8 terminal: {drawn}"
        );
    }

    #[test]
    fn the_trace_breaks_where_the_cliff_is() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let drawn = f.render(&[hit("a", 0.9, false, false), hit("b", 0.2, true, true)]);
        assert!(
            drawn.contains('┃') && drawn.contains('╵'),
            "the trace must snap: {drawn}"
        );
        // Drawn is not enough: a mark made only of glyphs is a mark a screen
        // reader never reaches.
        assert!(drawn.contains("past the cliff"), "{drawn}");
        assert!(drawn.contains("loose match"), "{drawn}");
    }

    #[test]
    fn a_face_that_is_off_renders_exactly_what_the_plain_renderer_does() {
        // The boundary that keeps the plain form the only one a script sees.
        let hits = [hit("a", 0.9, false, false), hit("b", 0.2, true, true)];
        let off = Face::decide(&Default::default(), false, false, Some("en_US.UTF-8"));
        assert_eq!(off.render(&hits), crate::cli::search::render_plain(&hits));
    }

    #[test]
    fn a_pulse_is_not_started_when_the_face_is_off() {
        let off = Face::decide(&Default::default(), false, false, None);
        assert!(off.pulse("searching").is_none());
    }

    /// POSIX precedence, which is not what this used to read.
    ///
    /// macOS commonly ships `LC_CTYPE=UTF-8` with no `LANG` at all, and those
    /// terminals were handed the ASCII understudies though they draw `▇` and
    /// `┃` perfectly. The other direction was wrong too: `LC_ALL=C` alongside
    /// a UTF-8 `LANG` got box-drawing it cannot render.
    #[test]
    fn the_locale_is_read_in_the_order_posix_says() {
        let env = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |k: &str| {
                owned
                    .iter()
                    .find(|(name, _)| name == k)
                    .map(|(_, v)| v.clone())
            }
        };

        assert_eq!(
            super::locale_from(env(&[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")])).as_deref(),
            Some("C"),
            "LC_ALL overrides everything"
        );
        assert_eq!(
            super::locale_from(env(&[("LC_CTYPE", "UTF-8")])).as_deref(),
            Some("UTF-8"),
            "LC_CTYPE with no LANG is the shipped macOS configuration"
        );
        assert_eq!(
            super::locale_from(env(&[("LANG", "de_DE.UTF-8")])).as_deref(),
            Some("de_DE.UTF-8"),
            "LANG is the fallback, not the first choice"
        );
        // What a shell means by `LC_ALL=`.
        assert_eq!(
            super::locale_from(env(&[("LC_ALL", ""), ("LANG", "en_US.UTF-8")])).as_deref(),
            Some("en_US.UTF-8"),
            "an empty value is unset"
        );
        assert_eq!(super::locale_from(env(&[])), None);

        // And the whole point of reading it: the glyphs follow.
        assert!(super::Face::decide(&always(), true, false, Some("UTF-8")).unicode);
        assert!(!super::Face::decide(&always(), true, false, Some("C")).unicode);
    }
}
