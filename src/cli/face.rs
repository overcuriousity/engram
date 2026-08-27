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

/// Where one background stage has got to.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Lamp {
    /// Not reached yet.
    Waiting,
    /// The stage the corpus is in right now.
    Running,
    /// Finished, whether this client watched it happen or it was already past.
    Done,
    /// Not finished, and not going to be.
    Stopped,
}

/// Extraction, segmentation and embedding — the three stages `-c --watch`
/// follows, drawn from the one status the server reports.
///
/// The mapping is a total function of the status rather than a tally this
/// client keeps, because a client that starts watching late has missed the
/// transitions and must still draw the truth: a corpus that is embedding was
/// segmented, whoever saw it happen.
pub struct Lamps(pub [Lamp; 3]);

impl Lamps {
    pub fn of(status: crate::store::corpora::CorpusStatus) -> Lamps {
        use crate::store::corpora::CorpusStatus as S;
        use Lamp::*;
        Lamps(match status {
            S::Describing | S::Extracting => [Running, Waiting, Waiting],
            S::Raw => [Done, Waiting, Waiting],
            S::Segmenting => [Done, Running, Waiting],
            S::Segmented => [Done, Done, Waiting],
            S::Embedding => [Done, Done, Running],
            S::Ready => [Done, Done, Done],
            // Stored on purpose without being segmented: the extraction is
            // real and finished, and the two stages after it are not coming.
            S::NeedsReview => [Done, Stopped, Stopped],
            // Some of it embedded and some of it did not.
            S::Partial => [Done, Done, Stopped],
            // Nothing can be claimed about a capture that failed.
            S::Failed => [Stopped, Stopped, Stopped],
        })
    }

    /// One line, drawn and said. The words are the claim; the glyphs are how
    /// you see at a glance which of the three is moving.
    pub fn render(&self, unicode: bool) -> String {
        let glyph = |l: Lamp| match (l, unicode) {
            (Lamp::Waiting, true) => '·',
            (Lamp::Running, true) => '◉',
            (Lamp::Done, true) => '●',
            (Lamp::Stopped, true) => '×',
            (Lamp::Waiting, false) => '.',
            (Lamp::Running, false) => 'o',
            (Lamp::Done, false) => 'O',
            (Lamp::Stopped, false) => 'x',
        };
        ["extract", "segment", "embed"]
            .iter()
            .zip(self.0)
            .map(|(name, lamp)| format!("{} {name}", glyph(lamp)))
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// A track that fills as a body is read out of this process and into the
/// request.
///
/// It reports bytes handed to the transport, which is what this client can
/// honestly know: it is not a claim about what the server has received, and
/// the word beside it says `read` rather than `sent` for that reason.
pub struct Fill {
    on: bool,
    unicode: bool,
    width: usize,
    drawn: bool,
}

impl Fill {
    /// What this chunk would draw, or `None` where the face is off.
    pub fn line(&self, done: usize, total: usize) -> Option<String> {
        if !self.on || total == 0 {
            return None;
        }
        let (full, empty) = if self.unicode {
            ('█', '░')
        } else {
            ('#', '.')
        };
        // A quarter of the terminal, so the track never crowds out the word
        // beside it on a narrow window.
        let cells = (self.width / 4).clamp(8, 40);
        let done = done.min(total);
        let lit = (cells * done) / total;
        let bar: String = (0..cells)
            .map(|i| if i < lit { full } else { empty })
            .collect();
        Some(format!(
            "{bar}  read {}%",
            (100 * done as u64) / total as u64
        ))
    }

    pub fn show(&mut self, done: usize, total: usize) {
        let Some(line) = self.line(done, total) else {
            return;
        };
        use std::io::Write;
        eprint!("\r\u{1b}[2K{line}");
        std::io::stderr().flush().ok();
        self.drawn = true;
    }

    pub fn clear(&mut self) {
        if self.drawn {
            use std::io::Write;
            eprint!("\r\u{1b}[2K");
            std::io::stderr().flush().ok();
            self.drawn = false;
        }
    }
}

/// The three lamps, drawn in place while a capture is watched.
///
/// Holds no timer and starts no thread: it is redrawn from each poll the watch
/// loop was already making, so nothing here can delay a result or outlive the
/// loop that owns it.
pub struct Track {
    on: bool,
    unicode: bool,
    drawn: bool,
}

impl Track {
    /// What this poll would draw, or `None` where the face is off.
    ///
    /// Separate from `show` because the rule worth asserting is that a pipe
    /// receives nothing, and stderr is a poor place to assert it from.
    pub fn line(&self, status: crate::store::corpora::CorpusStatus) -> Option<String> {
        self.on.then(|| Lamps::of(status).render(self.unicode))
    }

    /// Draw this poll's stage, over the last one.
    ///
    /// On stderr, and by rewriting the current line rather than on the
    /// alternate screen: the ids `-c` prints on stdout have to stay in
    /// scrollback and stay pipeable.
    pub fn show(&mut self, status: crate::store::corpora::CorpusStatus) {
        let Some(line) = self.line(status) else {
            return;
        };
        use std::io::Write;
        eprint!("\r\u{1b}[2K{line}");
        std::io::stderr().flush().ok();
        self.drawn = true;
    }

    /// Take the line back before anything else prints, or a result lands on
    /// top of half a track.
    pub fn clear(&mut self) {
        if self.drawn {
            use std::io::Write;
            eprint!("\r\u{1b}[2K");
            std::io::stderr().flush().ok();
            self.drawn = false;
        }
    }
}

/// How many cells the streaming readout is wide, and how long each one covers.
/// Ten hundred-millisecond buckets: a one-second window, which is short enough
/// that a model pausing to think shows as the readout falling away.
const READOUT_CELLS: usize = 10;
const READOUT_BUCKET_MS: u64 = 100;

/// An activity readout of a thing genuinely happening: the rate at which the
/// answer's own tokens are arriving.
///
/// Drawn at the cursor, immediately after the text written so far, and taken
/// back before the next text is written. That is the only place a line can be
/// redrawn without either disturbing what is already on the screen or moving to
/// the alternate screen, and the answer has to stay in scrollback.
///
/// The amplitude is measured, never invented. A readout that animated on a
/// timer would claim arrivals that did not happen, which is the one thing an
/// activity display must not do.
pub struct Readout {
    on: bool,
    unicode: bool,
    width: usize,
    /// When each recent token arrived. Pruned to the window on every push, so
    /// this holds at most a second of arrivals however long the answer is.
    marks: Vec<u64>,
    /// Column the cursor sits at, so the readout is dropped rather than wrapped
    /// when the sentence being written has filled the line.
    col: usize,
    /// Cells drawn last time, to be walked back over before anything else.
    tail: usize,
}

impl Readout {
    /// The text to write for one arriving chunk: what was drawn last taken
    /// back, the chunk itself, then the readout at the new cursor.
    ///
    /// `at_ms` is passed rather than read from the clock so the shape of a
    /// stream can be tested without one.
    pub fn push(&mut self, text: &str, at_ms: u64) -> String {
        if !self.on {
            return text.to_string();
        }
        let mut out = self.erase();
        out.push_str(text);
        self.advance(text);
        self.marks.push(at_ms);
        let window = READOUT_CELLS as u64 * READOUT_BUCKET_MS;
        self.marks.retain(|m| at_ms.saturating_sub(*m) < window);
        let strand = self.strand(at_ms);
        // Two spaces of gap, and only if the line has room. A readout that
        // wrapped would push the answer's own text down a line and leave the
        // walk-back pointing at the wrong row.
        if self.col + strand.chars().count() + 2 < self.width {
            out.push_str("  ");
            out.push_str(&strand);
            self.tail = strand.chars().count() + 2;
        }
        out
    }

    /// Take the readout back for good. Called before the sources are printed,
    /// so nothing of it survives into the scrollback.
    pub fn finish(&mut self) -> String {
        self.erase()
    }

    fn erase(&mut self) -> String {
        if self.tail == 0 {
            return String::new();
        }
        let back = self.tail;
        self.tail = 0;
        // Back over what was drawn, then erase to the end of the line: the
        // cursor is left exactly where the answer's own text ended.
        format!("\u{1b}[{back}D\u{1b}[K")
    }

    /// Where the cursor ends up after this chunk is written.
    fn advance(&mut self, text: &str) {
        match text.rsplit_once('\n') {
            Some((_, after)) => self.col = after.chars().count(),
            None => self.col += text.chars().count(),
        }
    }

    /// One cell per bucket of the window, oldest on the left, each cell as tall
    /// as that bucket was busy.
    fn strand(&self, now_ms: u64) -> String {
        let blocks = if self.unicode { BLOCKS } else { ASCII_BLOCKS };
        (0..READOUT_CELLS)
            .map(|i| {
                let age = (READOUT_CELLS - i) as u64 * READOUT_BUCKET_MS;
                let from = now_ms.saturating_sub(age);
                let to = from + READOUT_BUCKET_MS;
                let n = self
                    .marks
                    .iter()
                    .filter(|m| **m >= from && **m < to)
                    .count();
                match n {
                    0 => ' ',
                    n => blocks[(n - 1).min(blocks.len() - 1)],
                }
            })
            .collect()
    }
}

/// The span of one ranked list, and where a score sits in it.
///
/// Its own type because "best full, worst empty" is only meaningful relative to
/// a list, and because the degenerate list — one hit, or several that tied — is
/// a division by zero that has to be answered somewhere.
struct Scale {
    low: f32,
    span: f32,
}

impl Scale {
    fn over(hits: &[SearchResult]) -> Scale {
        let low = hits.iter().map(|h| h.score).fold(f32::INFINITY, f32::min);
        let high = hits
            .iter()
            .map(|h| h.score)
            .fold(f32::NEG_INFINITY, f32::max);
        Scale {
            low,
            span: high - low,
        }
    }

    /// Zero to seven. A list with no spread gets the top rung throughout:
    /// nothing separates those hits, and drawing them all empty would say the
    /// opposite of what a tie means.
    fn rung(&self, score: f32) -> usize {
        if !self.span.is_finite() || self.span <= f32::EPSILON {
            return 7;
        }
        ((((score - self.low) / self.span) * 7.0).round() as usize).min(7)
    }
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
        // The list is its own scale. A score here is a fused rank rather than a
        // probability — `search::prime` says so about the same numbers — and a
        // whole list of them is routinely negative, so a fixed `0.0..=1.0`
        // clamp put every hit on the bottom rung and the bar said nothing.
        let scale = Scale::over(hits);
        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            let rung = scale.rung(h.score);
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

    /// A readout of the rate an answer's tokens are arriving at.
    ///
    /// Off where the face is off, and then `push` hands back the text
    /// unchanged: `engram -a … | tee` writes the answer and nothing else.
    pub fn readout(&self) -> Readout {
        Readout {
            on: self.on,
            unicode: self.unicode,
            width: self.width,
            marks: Vec::new(),
            col: 0,
            tail: 0,
        }
    }

    /// A track for a body being read into a request.
    pub fn fill(&self) -> Fill {
        Fill {
            on: self.on,
            unicode: self.unicode,
            width: self.width,
            drawn: false,
        }
    }

    /// The three background stages, for a watch loop to redraw from its polls.
    pub fn track(&self) -> Track {
        Track {
            on: self.on,
            unicode: self.unicode,
            drawn: false,
        }
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

    /// A score here is a fused rank, not a probability, and a whole list of
    /// them is routinely negative — the shell that prompted this drew ten hits
    /// between -3.57 and -5.45. Clamping to `0.0..=1.0` gave every one of them
    /// the bottom rung, so the bar said nothing at all. The list is its own
    /// scale: best full, worst empty, and the steps between them real.
    /// The three background stages every other door only describes in a
    /// sentence. The corpus status names exactly one of them as running, and
    /// everything before it is finished by definition — a corpus cannot be
    /// embedding without having been segmented first.
    /// A face that is off draws no track at all — not an ASCII one, none.
    /// `-c --watch` down a pipe prints one status line per corpus and nothing
    /// else, which is what every script reading it was written against.
    #[test]
    fn a_track_is_not_drawn_where_the_face_is_off() {
        use crate::store::corpora::CorpusStatus as S;
        let off = Face::decide(&Default::default(), false, false, Some("en_US.UTF-8"));
        assert!(off.track().line(S::Segmenting).is_none());

        let on = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        assert!(on.track().line(S::Segmenting).is_some());
    }

    #[test]
    fn the_lamps_read_the_stage_the_corpus_is_actually_in() {
        use crate::store::corpora::CorpusStatus as S;
        use Lamp::*;
        assert_eq!(Lamps::of(S::Extracting).0, [Running, Waiting, Waiting]);
        assert_eq!(Lamps::of(S::Describing).0, [Running, Waiting, Waiting]);
        assert_eq!(Lamps::of(S::Raw).0, [Done, Waiting, Waiting]);
        assert_eq!(Lamps::of(S::Segmenting).0, [Done, Running, Waiting]);
        assert_eq!(Lamps::of(S::Segmented).0, [Done, Done, Waiting]);
        assert_eq!(Lamps::of(S::Embedding).0, [Done, Done, Running]);
        assert_eq!(Lamps::of(S::Ready).0, [Done, Done, Done]);
    }

    /// A capture that stopped did not finish the stage it stopped in, and a
    /// rendering that lit all three would say it did.
    #[test]
    fn a_capture_that_stopped_does_not_light_the_stage_it_never_reached() {
        use crate::store::corpora::CorpusStatus as S;
        use Lamp::*;
        assert_eq!(Lamps::of(S::Failed).0, [Stopped, Stopped, Stopped]);
        assert_eq!(
            Lamps::of(S::NeedsReview).0,
            [Done, Stopped, Stopped],
            "captured and stored, and deliberately not segmented"
        );
        assert_eq!(
            Lamps::of(S::Partial).0,
            [Done, Done, Stopped],
            "some of it embedded and some of it did not"
        );
    }

    /// Drawn and said, like every other claim this face makes: a lamp that is
    /// only a glyph is a lamp nobody reading a screen reader sees.
    #[test]
    fn the_lamps_name_their_stages_in_words() {
        use crate::store::corpora::CorpusStatus as S;
        let drawn = Lamps::of(S::Segmenting).render(true);
        for stage in ["extract", "segment", "embed"] {
            assert!(drawn.contains(stage), "{drawn}");
        }
    }

    #[test]
    fn a_terminal_that_cannot_draw_gets_the_same_lamps_in_ascii() {
        use crate::store::corpora::CorpusStatus as S;
        let drawn = Lamps::of(S::Embedding).render(false);
        assert!(drawn.is_ascii(), "a drawing glyph reached it: {drawn}");
        for stage in ["extract", "segment", "embed"] {
            assert!(drawn.contains(stage), "{drawn}");
        }
    }

    #[test]
    fn the_bar_is_drawn_against_the_list_it_is_in() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let drawn = f.render(&[
            hit("a", -3.57, false, false),
            hit("b", -4.42, false, false),
            hit("c", -5.45, false, false),
        ]);
        let rungs: Vec<char> = drawn
            .lines()
            .filter_map(|l| l.chars().find(|c| BLOCKS.contains(c)))
            .collect();
        assert_eq!(rungs.len(), 3, "one bar per hit: {drawn}");
        assert_eq!(rungs[0], '█', "the best hit of the list fills the bar");
        assert_eq!(rungs[2], '▁', "and the worst empties it");
        assert!(
            rungs[1] != rungs[0] && rungs[1] != rungs[2],
            "the middle hit got no step of its own: {drawn}"
        );
    }

    /// One hit, or several that tied, have no spread to be drawn against.
    /// Dividing by that spread is a division by zero.
    #[test]
    fn a_list_with_no_spread_still_draws() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let drawn = f.render(&[hit("a", -2.0, false, false), hit("b", -2.0, false, false)]);
        assert!(!drawn.contains("NaN"), "{drawn}");
        let rungs: Vec<char> = drawn
            .lines()
            .filter_map(|l| l.chars().find(|c| BLOCKS.contains(c)))
            .collect();
        assert_eq!(rungs.len(), 2, "{drawn}");
        assert_eq!(rungs[0], rungs[1], "nothing separates them: {drawn}");
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
