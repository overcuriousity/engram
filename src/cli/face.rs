//! What the terminal door looks like when a person is watching. It never survives a pipe,
//! never delays a result, and never says by colour or glyph alone what it must say in words.

use crate::cli::args::{CliArgs, Fancy};
use crate::core::search::SearchResult;

#[derive(Clone, Copy)]
pub struct Face {
    pub on: bool,
    /// The SGR escapes; apart from `on` because `NO_COLOR` is about ink, not lamps or layout.
    pub color: bool,
    pub unicode: bool,
    pub width: usize,
}

/// The eight rungs of a score bar and their ASCII understudies, so the list keeps its shape.
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Display lines of a hit's text: enough to tell two passages of one document apart.
const EXCERPT_LINES: usize = 4;
const ASCII_BLOCKS: [char; 8] = ['.', '.', ':', ':', '-', '=', '#', '#'];

const BAR_CELLS: usize = 7;

/// `s`, or as much as `room` holds plus the locale's ellipsis; a room of one or zero still clips.
pub(crate) fn clip(s: &str, room: usize, unicode: bool) -> String {
    if s.chars().count() <= room {
        return s.to_string();
    }
    let more = if unicode { "…" } else { "..." };
    match room.checked_sub(more.chars().count()) {
        // Narrower than the marker: as much of it as fits.
        None => more.chars().take(room).collect(),
        Some(keep) => s.chars().take(keep).chain(more.chars()).collect(),
    }
}

/// The ANSI 8 only: the person's theme maps these to inks that work on their ground.
const BOLD: &str = "1";
const DIM: &str = "2";
const GREEN: &str = "32";
const RED: &str = "31";
const YELLOW: &str = "33";
const CYAN: &str = "36";
const RESET: &str = "\u{1b}[0m";

/// The locale that decides whether glyphs are drawable, in POSIX order (`LC_ALL`, `LC_CTYPE`,
/// `LANG`); empty is unset. `get` is passed in so tests do not race on the environment.
pub fn locale_from(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .filter_map(get)
        .find(|v| !v.is_empty())
}

pub fn locale() -> Option<String> {
    locale_from(|k| std::env::var(k).ok())
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Lamp {
    Waiting,
    Running,
    /// Finished, whether this client watched it happen or not.
    Done,
    /// Not finished, and not going to be.
    Stopped,
}

/// Every escape this module can emit, taken back out; one copy so it cannot lag the vocabulary.
#[cfg(test)]
pub(crate) fn strip_sgr(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// One lamp, drawn; shared by the capture and search stages so the glyphs cannot drift.
pub(crate) fn lamp_glyph(l: Lamp, unicode: bool) -> char {
    match (l, unicode) {
        (Lamp::Waiting, true) => '·',
        (Lamp::Running, true) => '◉',
        (Lamp::Done, true) => '●',
        (Lamp::Stopped, true) => '×',
        (Lamp::Waiting, false) => '.',
        (Lamp::Running, false) => 'o',
        (Lamp::Done, false) => 'O',
        (Lamp::Stopped, false) => 'x',
    }
}

/// Extraction, segmentation and embedding as `-c --watch` draws them: a function of the status,
/// so a client that starts watching late still draws the truth.
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
            // Stored without segmenting: extraction finished, the next two are not coming.
            S::NeedsReview => [Done, Stopped, Stopped],
            S::Partial => [Done, Done, Stopped],
            S::Failed => [Stopped, Stopped, Stopped],
        })
    }

    /// One line, drawn and said: the words are the claim, the glyphs a glance.
    pub fn render(&self, unicode: bool) -> String {
        ["extract", "segment", "embed"]
            .iter()
            .zip(self.0)
            .map(|(name, lamp)| format!("{} {name}", lamp_glyph(lamp, unicode)))
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// A track that fills as a body is handed to the transport (`read`, not a claim the server got it).
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
        // A quarter of the terminal, so the track never crowds out its word.
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

/// Taken back on drop: a request that fails mid-upload never reaches its own `clear`.
impl Drop for Fill {
    fn drop(&mut self) {
        self.clear();
    }
}

/// The three lamps, redrawn from each poll the watch loop makes: no timer, no thread.
pub struct Track {
    on: bool,
    unicode: bool,
    drawn: bool,
}

impl Track {
    /// What this poll would draw, or `None` where the face is off; testable apart from stderr.
    pub fn line(&self, status: crate::store::corpora::CorpusStatus) -> Option<String> {
        self.on.then(|| Lamps::of(status).render(self.unicode))
    }

    /// Draw this poll's stage over the last one, on stderr: the ids on stdout stay pipeable.
    pub fn show(&mut self, status: crate::store::corpora::CorpusStatus) {
        let Some(line) = self.line(status) else {
            return;
        };
        use std::io::Write;
        eprint!("\r\u{1b}[2K{line}");
        std::io::stderr().flush().ok();
        self.drawn = true;
    }

    /// Take the line back before anything else prints.
    pub fn clear(&mut self) {
        if self.drawn {
            use std::io::Write;
            eprint!("\r\u{1b}[2K");
            std::io::stderr().flush().ok();
            self.drawn = false;
        }
    }
}

/// The stages of one search, redrawn only as the server's frames arrive. No quiet floor: it
/// would hide the slow first stage, the case this display exists for.
pub struct Stages {
    face: Face,
    names: Vec<crate::core::search::SearchStage>,
    drawn: bool,
}

impl Stages {
    pub fn start(&mut self, names: &[crate::core::search::SearchStage]) {
        self.names = names.to_vec();
    }

    /// What this frame would draw, or `None` where the face is off.
    pub fn at(&self, now: crate::core::search::SearchStage) -> Option<String> {
        use crate::core::search::SearchStage;
        if !self.face.on || self.names.is_empty() {
            return None;
        }
        let mut reached = false;
        let lamps: Vec<(Lamp, &str)> = self
            .names
            .iter()
            .map(|s| {
                // Before the running stage is done, after it is not, however late the watch began.
                let lamp = match (*s == now, reached) {
                    (true, _) => {
                        reached = true;
                        Lamp::Running
                    }
                    (false, false) => Lamp::Done,
                    (false, true) => Lamp::Waiting,
                };
                let name = match s {
                    SearchStage::Embed => "embed",
                    SearchStage::Retrieve => "retrieve",
                    SearchStage::Rerank => "rerank",
                };
                (lamp, name)
            })
            .collect();
        // Counted before the ink: escapes make a character count no column count.
        let room = self.face.width.saturating_sub(2);
        // Two columns for each lamp and its space, three between neighbours.
        let wide: usize = lamps
            .iter()
            .map(|(_, n)| n.chars().count() + 2)
            .sum::<usize>()
            + 3 * lamps.len().saturating_sub(1);
        // Too narrow: the running stage alone; `show` erases one row, so a wrap would stay.
        if wide > room {
            let (lamp, name) = lamps
                .iter()
                .find(|(l, _)| *l == Lamp::Running)
                .or_else(|| lamps.last())?;
            let said = clip(name, room.saturating_sub(2), self.face.unicode);
            return Some(self.face.lamp_line(*lamp, &said));
        }
        Some(
            lamps
                .iter()
                .map(|(lamp, name)| self.face.lamp_line(*lamp, name))
                .collect::<Vec<_>>()
                .join("   "),
        )
    }

    /// Draw this frame's stage over the last one, on stderr, so the results stay pipeable.
    pub fn show(&mut self, now: crate::core::search::SearchStage) {
        let Some(line) = self.at(now) else {
            return;
        };
        use std::io::Write;
        eprint!("\r\u{1b}[2K  {line}");
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

/// Taken back on drop, as `Fill` is.
impl Drop for Stages {
    fn drop(&mut self) {
        self.clear();
    }
}

/// The streaming readout's cells and span: a one-second window, so a pause shows as a fall.
const READOUT_CELLS: usize = 10;
const READOUT_BUCKET_MS: u64 = 100;

/// The rate the answer's own tokens arrive at, drawn at the cursor and taken back before the next
/// text. Measured, never animated on a timer.
pub struct Readout {
    on: bool,
    unicode: bool,
    width: usize,
    /// When each recent token arrived, pruned to the window on every push.
    marks: Vec<u64>,
    /// Cursor column, so the readout is dropped rather than wrapped on a full line.
    col: usize,
    /// Cells drawn last time, to be walked back over.
    tail: usize,
}

impl Readout {
    /// The text for one arriving chunk: last drawing taken back, the chunk, the readout. `at_ms` is
    /// passed so tests need no clock.
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
        // Only if the line has room: a wrapped readout leaves the walk-back on the wrong row.
        if self.col + strand.chars().count() + 2 < self.width {
            out.push_str("  ");
            out.push_str(&strand);
            self.tail = strand.chars().count() + 2;
        }
        out
    }

    /// Take the readout back for good, before the sources print.
    pub fn finish(&mut self) -> String {
        self.erase()
    }

    fn erase(&mut self) -> String {
        if self.tail == 0 {
            return String::new();
        }
        let back = self.tail;
        self.tail = 0;
        // Back over what was drawn, then erase to end of line.
        format!("\u{1b}[{back}D\u{1b}[K")
    }

    fn advance(&mut self, text: &str) {
        match text.rsplit_once('\n') {
            Some((_, after)) => self.col = after.chars().count(),
            None => self.col += text.chars().count(),
        }
    }

    /// One cell per bucket, oldest left, as tall as the bucket was busy; bucketed by age from `now`
    /// so the token at `now` lands in the newest cell.
    fn strand(&self, now_ms: u64) -> String {
        let blocks = if self.unicode { BLOCKS } else { ASCII_BLOCKS };
        let mut counts = [0usize; READOUT_CELLS];
        for m in &self.marks {
            // A mark from ahead of `now` (a clock stepped back) saturates to the newest cell.
            let back = (now_ms.saturating_sub(*m) / READOUT_BUCKET_MS) as usize;
            if back < READOUT_CELLS {
                counts[READOUT_CELLS - 1 - back] += 1;
            }
        }
        counts
            .iter()
            .map(|n| match n {
                0 => ' ',
                n => blocks[(n - 1).min(blocks.len() - 1)],
            })
            .collect()
    }
}

/// The span of one ranked list, and where a score sits in it; it answers the one-hit or tied case.
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

    /// Zero to seven; a list with no spread gets the top rung throughout, as a tie means.
    fn rung(&self, score: f32) -> usize {
        if !self.span.is_finite() || self.span <= f32::EPSILON {
            return 7;
        }
        ((((score - self.low) / self.span) * 7.0).round() as usize).min(7)
    }
}

impl Face {
    /// Facts passed in, not read, so every rule is testable without a terminal; `lang` is the whole
    /// locale from `locale`, not `LANG`.
    pub fn decide(cli: &CliArgs, is_tty: bool, no_color: bool, lang: Option<&str>) -> Face {
        let on = match cli.fancy {
            Fancy::Always => true,
            Fancy::Never => false,
            // `--json`'s reader is a machine even when a person watches.
            Fancy::Auto => is_tty && !cli.plain && !cli.json,
        };
        Face {
            on,
            // `--fancy always` says draw, which is not saying colour.
            color: on && !no_color,
            unicode: lang.is_some_and(|l| l.to_ascii_uppercase().contains("UTF-8")),
            width: crossterm::terminal::size()
                .map(|(w, _)| w as usize)
                .unwrap_or(80),
        }
    }

    /// `s` wrapped in an SGR code, or untouched: the one place an escape is written.
    pub fn ink(&self, code: &str, s: &str) -> String {
        match self.color {
            true => format!("\u{1b}[{code}m{s}{RESET}"),
            false => s.to_string(),
        }
    }

    /// Recessive: an id, a timing, a row that is already past.
    pub fn ink_dim(&self, s: &str) -> String {
        self.ink(DIM, s)
    }

    /// Something that failed, and can be acted on.
    pub fn ink_bad(&self, s: &str) -> String {
        self.ink(RED, s)
    }

    /// Something waiting on a person: a capture held for review, a partial embedding.
    pub fn ink_caution(&self, s: &str) -> String {
        self.ink(YELLOW, s)
    }

    /// One lamp and the words beside it: `● retrieved 24, showing 6`.
    pub fn lamp_line(&self, lamp: Lamp, said: &str) -> String {
        let cell = format!("{} {said}", lamp_glyph(lamp, self.unicode));
        let code = match lamp {
            Lamp::Running => CYAN,
            Lamp::Done => GREEN,
            Lamp::Stopped => RED,
            Lamp::Waiting => DIM,
        };
        self.ink(code, &cell)
    }

    /// The ranked list, drawn; the plain renderer when the face is off. `elapsed_ms` is the
    /// client's timing, `None` where it took none.
    pub fn render(&self, hits: &[SearchResult], elapsed_ms: Option<u128>) -> String {
        if !self.on {
            return crate::cli::search::render_plain(hits);
        }
        let (full, empty) = if self.unicode {
            ('▇', '░')
        } else {
            ('#', '.')
        };
        // The list is its own scale: scores are fused ranks, often all negative, not probabilities.
        let scale = Scale::over(hits);
        let mut out = String::new();

        // The query is not echoed: the terminal shows the command above.
        let mut head = format!(
            "{} hit{}",
            hits.len(),
            if hits.len() == 1 { "" } else { "s" }
        );
        if let Some(ms) = elapsed_ms {
            head.push_str(&format!(" · {ms} ms"));
        }
        out.push_str(&format!("\n  {}\n\n", self.ink(DIM, &head)));

        let mut said_cliff = false;
        for (i, h) in hits.iter().enumerate() {
            // Said once, in words, where the fall happens.
            if h.past_cliff && !said_cliff {
                said_cliff = true;
                out.push_str(&format!("  {}\n\n", self.ink(CYAN, &self.divider())));
            }
            // Every hit was returned, so at least one cell; only the bottom rung is lifted.
            let lit = scale.rung(h.score).max(1);
            let bar: String = (0..BAR_CELLS)
                .map(|c| if c < lit { full } else { empty })
                .collect();
            // Eight characters: what `last::resolve` accepts as a prefix.
            let short: String = h.artifact_id.chars().take(8).collect();
            // 26 columns: indent 2, rank 2, gap 2, bar 7, gap 2, 3, short id 8. One less wraps.
            let title = clip(
                h.title.as_deref().unwrap_or("(untitled)"),
                self.width.saturating_sub(26),
                self.unicode,
            );
            let (bar, title) = match h.past_cliff {
                true => (self.ink(DIM, &bar), self.ink(DIM, &title)),
                false => (self.ink(CYAN, &bar), self.ink(BOLD, &title)),
            };
            out.push_str(&format!(
                "  {:>2}  {bar}  {title}   {}\n",
                i + 1,
                self.ink(DIM, &short)
            ));
            if let Some(said) = crate::cli::search::badges(h) {
                out.push_str(&format!(
                    "      {}\n",
                    self.ink(DIM, &said.replace(", ", " · "))
                ));
            }
            // Four wrapped display lines, not four source lines.
            for line in
                crate::cli::search::excerpt(&h.text, EXCERPT_LINES, self.width.saturating_sub(6))
            {
                out.push_str(&format!("      {line}\n"));
            }
            out.push('\n');
        }
        // Only where it fits: a hint that wraps is worse than none.
        if !hits.is_empty() && self.width >= 60 {
            out.push_str(&format!(
                "  {}\n",
                self.ink(DIM, "engram --show 1   reads the first hit in full")
            ));
        }
        out
    }

    /// The cliff, as a rule with the sentence in it; a narrow terminal gets the sentence alone.
    fn divider(&self) -> String {
        const SAID: &str = "relevance falls off here";
        let dash = if self.unicode { '─' } else { '-' };
        let room = self.width.saturating_sub(2);
        match room.checked_sub(SAID.chars().count() + 4) {
            Some(n) if n >= 6 => {
                let side: String = std::iter::repeat_n(dash, n / 2).collect();
                format!("{side}  {SAID}  {side}")
            }
            _ => SAID.to_string(),
        }
    }

    /// A readout of the token rate; off with the face, so `-a … | tee` writes only the answer.
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

    pub fn stages(&self) -> Stages {
        Stages {
            face: *self,
            names: Vec::new(),
            drawn: false,
        }
    }

    pub fn fill(&self) -> Fill {
        Fill {
            on: self.on,
            unicode: self.unicode,
            width: self.width,
            drawn: false,
        }
    }

    pub fn track(&self) -> Track {
        Track {
            on: self.on,
            unicode: self.unicode,
            drawn: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{CliArgs, Fancy};
    use crate::cli::search::fixture::hit;

    fn readout() -> Readout {
        Readout {
            on: true,
            unicode: true,
            width: 200,
            marks: Vec::new(),
            col: 0,
            tail: 0,
        }
    }

    #[test]
    fn the_token_that_triggered_the_redraw_is_in_the_strand() {
        let mut r = readout();
        r.marks.push(1_000);
        let strand = r.strand(1_000);
        assert_eq!(strand.chars().count(), READOUT_CELLS);
        assert_ne!(
            strand.chars().next_back(),
            Some(' '),
            "the newest cell is empty on the very tick a token arrived: {strand:?}"
        );
        assert!(
            strand.chars().take(READOUT_CELLS - 1).all(|c| c == ' '),
            "one mark lit more than its own cell: {strand:?}"
        );
    }

    #[test]
    fn the_first_second_of_a_stream_draws_no_duplicate_cells() {
        let mut r = readout();
        r.marks.push(10);
        for now in [10, 50, 99, 150, 300, 999] {
            let strand = r.strand(now);
            assert_eq!(
                strand.chars().filter(|c| *c != ' ').count(),
                1,
                "one mark lit several cells at {now}ms: {strand:?}"
            );
        }
    }

    #[test]
    fn a_mark_older_than_the_window_is_not_drawn() {
        let mut r = readout();
        r.marks.push(0);
        let window = READOUT_CELLS as u64 * READOUT_BUCKET_MS;
        assert!(r.strand(window).chars().all(|c| c == ' '));
    }

    fn lit_cells(drawn: &str) -> Vec<usize> {
        drawn
            .lines()
            .map(|l| l.chars().filter(|c| *c == '▇').count())
            .filter(|n| *n > 0)
            .collect()
    }

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
        // Long enough to be clipped, so the ellipsis glyph is exercised.
        let long = "Betriebssysteme fuer Server und die Dienste die auf ihnen laufen";
        let drawn = f.render(&[hit(long, 0.9, false, false)], None);
        assert!(
            drawn.is_ascii(),
            "a drawing glyph reached a non-UTF-8 terminal: {drawn}"
        );
    }

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
        let drawn = f.render(
            &[
                hit("a", -3.57, false, false),
                hit("b", -4.42, false, false),
                hit("c", -5.45, false, false),
            ],
            None,
        );
        let bars = lit_cells(&drawn);
        assert_eq!(bars.len(), 3, "one bar per hit: {drawn}");
        assert_eq!(bars[0], BAR_CELLS, "the best hit of the list fills the bar");
        assert_eq!(bars[2], 1, "and the worst is down to its last cell");
        assert!(
            bars[1] != bars[0] && bars[1] != bars[2],
            "the middle hit got no step of its own: {drawn}"
        );
    }

    #[test]
    fn a_list_with_no_spread_still_draws() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let drawn = f.render(
            &[hit("a", -2.0, false, false), hit("b", -2.0, false, false)],
            None,
        );
        assert!(!drawn.contains("NaN"), "{drawn}");
        let bars = lit_cells(&drawn);
        assert_eq!(bars.len(), 2, "{drawn}");
        assert_eq!(bars[0], bars[1], "nothing separates them: {drawn}");
    }

    #[test]
    fn the_cliff_is_said_where_it_falls() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let drawn = f.render(
            &[hit("a", 0.9, false, false), hit("b", 0.2, true, true)],
            None,
        );
        // A glyph-only mark never reaches a screen reader, a screenshot or a monochrome terminal.
        assert!(drawn.contains("relevance falls off here"), "{drawn}");
        assert!(
            !drawn.contains('┃') && !drawn.contains('╵'),
            "the trace says nothing the divider does not: {drawn}"
        );
        assert!(drawn.contains("past the cliff"), "{drawn}");
        assert!(drawn.contains("loose match"), "{drawn}");
    }

    #[test]
    fn a_list_that_does_not_fall_off_has_no_divider() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let drawn = f.render(&[hit("a", 0.9, false, false)], None);
        assert!(!drawn.contains("relevance falls off"), "{drawn}");
    }

    #[test]
    fn the_raw_score_leaves_the_human_rendering() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let drawn = f.render(&[hit("a", -3.57, false, false)], None);
        assert!(!drawn.contains("-3.57"), "{drawn}");
        assert!(drawn.contains('▇'), "{drawn}");
    }

    #[test]
    fn the_id_is_cut_to_a_prefix_that_still_names_it() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let mut h = hit("a", 0.9, false, false);
        h.artifact_id = "01J8Z4K2QW7NR3T9X0YB5C6D8E".into();
        let drawn = f.render(&[h], None);
        assert!(drawn.contains("01J8Z4K2"), "{drawn}");
        assert!(
            !drawn.contains("01J8Z4K2QW"),
            "the whole id crowds the title: {drawn}"
        );
    }

    #[test]
    fn the_header_says_what_was_counted_and_what_was_timed() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let drawn = f.render(
            &[hit("a", 0.9, false, false), hit("b", 0.2, false, false)],
            Some(412),
        );
        assert!(drawn.contains("2 hits"), "{drawn}");
        assert!(drawn.contains("412 ms"), "{drawn}");
        let untimed = f.render(&[hit("a", 0.9, false, false)], None);
        assert!(untimed.contains("1 hit"), "{untimed}");
        assert!(!untimed.contains(" ms"), "{untimed}");
    }

    #[test]
    fn a_long_line_is_wrapped_onto_the_budget_rather_than_cut_at_the_edge() {
        let f = Face {
            on: true,
            color: false,
            unicode: true,
            width: 40,
        };
        let mut h = hit("a", -4.18, false, false);
        h.text = "Dienste bezeichnen Anwendungen ohne eine graphische \
                  Oberflaeche. Sie sind fuer periodische und zeitaufwendige \
                  Aufgaben geeignet."
            .into();
        let drawn = f.render(&[h], None);
        assert!(
            drawn.contains("zeitaufwendige"),
            "the tail was cut: {drawn}"
        );
        for line in drawn.lines() {
            assert!(line.chars().count() <= 40, "over the width: {line:?}");
        }
    }

    #[test]
    fn a_title_that_fills_the_row_is_clipped_rather_than_wrapped() {
        for width in [40usize, 60, 80, 120] {
            let f = Face {
                on: true,
                color: false,
                unicode: true,
                width,
            };
            for len in [width.saturating_sub(26), width, width + 20] {
                // Wrappable words: an unbreakable token would fail on the excerpt, not the title.
                let title: String = "wort ".repeat(width).chars().take(len.max(1)).collect();
                let drawn = f.render(&[hit(&title, -4.18, false, false)], None);
                for line in drawn.lines() {
                    assert!(
                        line.chars().count() <= width,
                        "width {width}, title of {len}: over the width: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn clipping_into_no_room_at_all_still_clips() {
        assert_eq!(clip("Dienste", 7, true), "Dienste");
        assert_eq!(clip("Dienste", 8, true), "Dienste");
        assert_eq!(clip("Dienste", 6, true), "Diens…");
        assert_eq!(clip("Dienste", 2, true), "D…");
        assert_eq!(clip("Dienste", 1, true), "…");
        assert_eq!(clip("Dienste", 0, true), "");
        assert_eq!(clip("", 0, true), "");
    }

    #[test]
    fn a_clip_on_an_ascii_terminal_ends_in_ascii() {
        for room in 0..8 {
            let out = clip("Dienste", room, false);
            assert!(out.is_ascii(), "room {room}: {out:?}");
            assert!(out.chars().count() <= room, "room {room}: {out:?}");
        }
        assert_eq!(clip("Dienste", 6, false), "Die...");
        assert_eq!(clip("Dienste", 2, false), "..");
    }

    #[test]
    fn the_lines_a_hit_is_clipped_to_are_lines_with_something_on_them() {
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let mut h = hit("a", -4.18, false, false);
        h.text = "Betriebssysteme f\u{fc}r Server\n\nDienste bezeichnen Anwendungen.\n".into();
        let drawn = f.render(&[h], None);
        assert!(drawn.contains("Betriebssysteme"), "{drawn}");
        assert!(
            drawn.contains("Dienste bezeichnen"),
            "second line lost: {drawn}"
        );
    }

    #[test]
    fn a_face_that_is_off_renders_exactly_what_the_plain_renderer_does() {
        let hits = [hit("a", 0.9, false, false), hit("b", 0.2, true, true)];
        let off = Face::decide(&Default::default(), false, false, Some("en_US.UTF-8"));
        assert_eq!(
            off.render(&hits, None),
            crate::cli::search::render_plain(&hits)
        );
    }

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

        assert!(super::Face::decide(&always(), true, false, Some("UTF-8")).unicode);
        assert!(!super::Face::decide(&always(), true, false, Some("C")).unicode);
    }

    fn drawn(color: bool) -> Face {
        Face {
            on: true,
            color,
            unicode: true,
            width: 80,
        }
    }

    #[test]
    fn colour_never_carries_a_claim_on_its_own() {
        let hits = [hit("a", 0.9, false, false), hit("b", 0.2, true, true)];
        assert_eq!(
            strip_sgr(&drawn(true).render(&hits, Some(412))),
            drawn(false).render(&hits, Some(412))
        );
    }

    #[test]
    fn ink_is_a_no_op_without_colour() {
        assert_eq!(drawn(false).ink(CYAN, "x"), "x");
        assert_eq!(drawn(true).ink(CYAN, "x"), "\u{1b}[36mx\u{1b}[0m");
    }

    #[test]
    fn a_stage_line_names_every_stage_and_marks_the_one_that_is_running() {
        use crate::core::search::SearchStage::*;
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let mut s = f.stages();
        s.start(&[Embed, Retrieve, Rerank]);
        let line = s.at(Retrieve).expect("a line");
        assert!(line.contains("embed") && line.contains("retrieve") && line.contains("rerank"));
        assert!(
            line.contains('●'),
            "the stage before it is finished: {line}"
        );
        assert!(line.contains('◉'), "the stage running: {line}");
        assert!(line.contains('·'), "the stage still to come: {line}");
    }

    #[test]
    fn only_the_stages_the_server_named_are_drawn() {
        use crate::core::search::SearchStage::*;
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let mut s = f.stages();
        s.start(&[Embed, Retrieve]);
        let line = s.at(Retrieve).expect("a line");
        assert!(!line.contains("rerank"), "{line}");
    }

    #[test]
    fn the_first_stage_is_drawn_as_soon_as_it_is_reported() {
        use crate::core::search::SearchStage::*;
        let f = Face::decide(&always(), true, false, Some("en_US.UTF-8"));
        let mut s = f.stages();
        s.start(&[Embed, Retrieve]);
        assert!(s.at(Embed).is_some());
    }

    #[test]
    fn a_stage_line_wider_than_the_terminal_is_cut_down_rather_than_wrapped() {
        use crate::core::search::SearchStage::*;
        for width in 4..40 {
            let f = Face {
                width,
                ..Face::decide(&always(), true, false, Some("en_US.UTF-8"))
            };
            let mut s = f.stages();
            s.start(&[Embed, Retrieve, Rerank]);
            let line = s.at(Retrieve).expect("a line");
            assert!(
                strip_sgr(&line).chars().count() + 2 <= width,
                "width {width}: {line:?}"
            );
            assert!(
                width < 10 || strip_sgr(&line).contains("retr"),
                "width {width}: the running stage is the one it keeps: {line:?}"
            );
        }
    }

    #[test]
    fn a_face_that_is_off_draws_no_stage_at_all() {
        use crate::core::search::SearchStage::*;
        let off = Face::decide(&Default::default(), false, false, None);
        let mut s = off.stages();
        s.start(&[Embed, Retrieve]);
        assert!(s.at(Embed).is_none());
    }

    #[test]
    fn no_color_keeps_the_layout_and_drops_only_the_ink() {
        let f = Face::decide(&Default::default(), true, true, Some("en_US.UTF-8"));
        assert!(f.on, "the lamps and the layout are not colour");
        assert!(!f.color);
    }

    #[test]
    fn asking_for_the_drawn_form_does_not_override_no_color() {
        let f = Face::decide(&always(), false, true, Some("en_US.UTF-8"));
        assert!(f.on);
        assert!(!f.color, "--fancy always says draw, not colour");
    }

    #[test]
    fn nothing_is_drawn_and_nothing_is_coloured() {
        let plain = CliArgs {
            plain: true,
            ..Default::default()
        };
        let json = CliArgs {
            json: true,
            ..Default::default()
        };
        let never = CliArgs {
            fancy: Fancy::Never,
            ..Default::default()
        };
        for (args, tty) in [
            (plain, true),
            (json, true),
            (CliArgs::default(), false),
            (never, true),
        ] {
            let f = Face::decide(&args, tty, false, Some("en_US.UTF-8"));
            assert!(!f.on);
            assert!(!f.color);
        }
    }
}
