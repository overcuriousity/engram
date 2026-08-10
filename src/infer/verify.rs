//! Does the chunk still say what the source said?
//!
//! The synthesizer is instructed to reproduce commands, paths and error strings
//! verbatim while rewriting the prose around them. Nothing checked that it
//! did, and a paraphrased command is a command that later gets pasted into a
//! root shell. These are pure functions over two strings, so they can be
//! tested exhaustively without a model.

/// A chunk contains a command, path or flag that its window does not.
pub const FLAG_LITERALS: &str = "literals_unverified";

/// Collapse whitespace runs so an indented source line and a fenced chunk line
/// compare equal. Anything else — a changed flag, a renamed path — still differs.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

const TRIM: [char; 8] = ['(', ')', ',', '.', ';', ':', '"', '\''];

fn looks_like_a_path_or_flag(token: &str) -> bool {
    let t = token.trim_matches(|c: char| TRIM.contains(&c));
    if t.len() < 3 {
        return false;
    }
    if t.starts_with("--") || t.starts_with('/') || t.starts_with("~/") || t.starts_with("./") {
        return true;
    }
    // A slash alone is not a path: ordinary prose is full of "enables/disables"
    // and "and/or", and flagging those buries the real misses under noise. A
    // relative path carries something else machine-shaped as well.
    t.contains('/') && t.contains(['.', '-', '_', '=', '$', '*'])
}

/// Every string in a chunk that must have come from the source verbatim:
/// lines inside fenced code blocks, inline code spans, and bare path- or
/// flag-shaped tokens in the prose.
pub fn extract_literals(artifact_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in artifact_text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        // Fenced blocks, and the indented kind markdown also treats as code —
        // reference documentation is full of the latter, and a command that
        // arrives indented rather than fenced still has to be verbatim.
        if fenced || line.starts_with("    ") || line.starts_with('\t') {
            if !line.trim().is_empty() {
                out.push(line.trim().to_string());
            }
            continue;
        }

        // Inline code spans, and the prose between them.
        let mut rest = line;
        let mut prose = String::new();
        while let Some(open) = rest.find('`') {
            prose.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            match after.find('`') {
                Some(close) => {
                    let span = after[..close].trim();
                    if !span.is_empty() {
                        out.push(span.to_string());
                    }
                    rest = &after[close + 1..];
                }
                None => break,
            }
        }
        prose.push_str(rest);

        // Bare paths and flags outside code spans.
        for token in prose.split_whitespace() {
            if looks_like_a_path_or_flag(token) {
                out.push(token.trim_matches(|c: char| TRIM.contains(&c)).to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Literals present in the chunk or its caveats and absent from the window they
/// came from.
///
/// Caveats go through the same check rather than a weaker one. They are the
/// newest place model prose can appear, and a caveat that says to run something
/// first is a command that gets pasted into a root shell exactly like one in
/// the body.
pub fn missing_literals(
    artifact_text: &str,
    caveats: &[String],
    segment_text: &str,
) -> Vec<String> {
    let haystack = normalize(segment_text);
    let mut all = extract_literals(artifact_text);
    for c in caveats {
        all.extend(extract_literals(c));
    }
    all.sort();
    all.dedup();
    all.into_iter()
        .filter(|lit| !haystack.contains(&normalize(lit)))
        .collect()
}

/// Where in the window a chunk's own lines actually appear.
///
/// The synthesizer is asked for `corpus_lines` and frequently omits them. Falling
/// back to the whole window is honest but useless: the detail pane then marks
/// every line as the span, which points at nothing. Matching the chunk's lines
/// against the window recovers a real span for anything the model reproduced
/// verbatim, which is precisely the commands and paths worth pointing at.
///
/// Returns `None` when nothing matches, and the caller keeps the window.
pub fn locate_span(
    artifact_text: &str,
    segment_body: &str,
    segment_start: i64,
) -> Option<(i64, i64)> {
    let flat = Flattened::of(segment_body);
    let needles: Vec<String> = artifact_text
        .lines()
        .map(normalize)
        .filter(|l| l.len() > 8)
        .collect();
    if needles.is_empty() {
        return None;
    }

    let mut first = usize::MAX;
    let mut last = 0usize;
    for needle in &needles {
        if let Some(at) = flat.text.find(needle.as_str()) {
            first = first.min(flat.line_at(at));
            last = last.max(flat.line_at(at + needle.len() - 1));
        }
    }
    if first == usize::MAX {
        return None;
    }
    Some((segment_start + first as i64, segment_start + last as i64))
}

/// A window with its line breaks taken out, and the map back.
///
/// Comparing a chunk's lines against the window's lines only finds what the
/// source happened to fit on one line. Sources do not cooperate: a handout
/// exported from a PDF is hard-wrapped at eighty columns, so one sentence is
/// three lines, and synthesis reflows it back into one. Line against line, the
/// paragraph matches nothing and a chunk built from it claims either a single
/// short sentence or no span at all — which is then what coverage counts.
///
/// Matching against the whole window as one string finds the paragraph, and the
/// offsets say which lines it ran across.
struct Flattened {
    text: String,
    /// Byte offset in `text` at which each source line begins.
    starts: Vec<usize>,
}

impl Flattened {
    fn of(segment_body: &str) -> Self {
        let mut text = String::new();
        let mut starts = Vec::new();
        for line in segment_body.lines() {
            let n = normalize(line);
            // The separator stands in for the line break the source had, so a
            // sentence reflowed across two lines reads as it does in the chunk.
            if !text.is_empty() && !n.is_empty() {
                text.push(' ');
            }
            starts.push(text.len());
            text.push_str(&n);
        }
        Self { text, starts }
    }

    /// Which line a byte offset falls in. The last line that starts at or
    /// before it — blank lines share their neighbour's offset and never win,
    /// because a match cannot begin inside one.
    fn line_at(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            // Several lines can share a start offset; take the last, which is
            // the one that actually holds text.
            Ok(mut i) => {
                while i + 1 < self.starts.len() && self.starts[i + 1] == offset {
                    i += 1;
                }
                i
            }
            Err(0) => 0,
            Err(i) => i - 1,
        }
    }
}

/// Below this fraction of a source inside some chunk, the segmenter probably
/// dropped part of the document.
pub const LOW_COVERAGE: f64 = 0.6;

fn distinctive_tokens(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '=')))
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| t.len() > 3)
        .collect()
}

/// Does the chunk plausibly describe the lines it claims?
///
/// The synthesizer rewrites prose, so this cannot demand equality — only that a
/// third of the chunk's distinctive tokens appear in the claimed range.
///
/// Synthesis no longer calls this: it derives spans rather than checking the
/// model's, so there is no claim left to doubt. It stays because
/// `content_coverage` measures the same relationship — is this text in those
/// lines — and the two want to keep answering it the same way.
pub fn span_is_plausible(artifact_text: &str, claimed_text: &str) -> bool {
    let chunk = distinctive_tokens(artifact_text);
    if chunk.is_empty() {
        return true;
    }
    let claimed = distinctive_tokens(claimed_text);
    let shared = chunk.iter().filter(|t| claimed.contains(*t)).count();
    shared * 3 >= chunk.len()
}

/// A source line counts as covered when this share of its distinctive tokens
/// appears in the artifacts made from the segment it belongs to.
///
/// Half, because synthesis rewrites: a line survives as its subject and its
/// values, not as its wording. Demanding all of it would call every rewritten
/// line lost, which is what the artifacts are supposed to be.
const LINE_TOKEN_RECALL: f64 = 0.5;

/// Fraction of the source that survived into some artifact.
///
/// Each entry is one segment's line range and the text of every artifact made
/// from it. A line outside every range — a segment that failed, or one never
/// attempted — is uncovered, which is exactly the case this number exists to
/// make visible.
///
/// This asks whether the *content* arrived, not whether an artifact claimed the
/// line. Claims were the earlier measure and they answer a different question:
/// the model omits `corpus_lines` more often than not, and a span recovered by
/// matching verbatim text finds only the quarter of an artifact that was not
/// rewritten. A faithfully rewritten chapter therefore scored near zero, which
/// read exactly like a chapter that had been dropped.
pub fn content_coverage(raw_text: &str, segments: &[(i64, i64, String)]) -> f64 {
    let lines: Vec<&str> = raw_text.lines().collect();
    let total = lines.iter().filter(|l| !l.trim().is_empty()).count();
    if total == 0 {
        return 0.0;
    }
    let indexed: Vec<(i64, i64, std::collections::HashSet<String>)> = segments
        .iter()
        .map(|(a, b, text)| (*a, *b, distinctive_tokens(text)))
        .collect();

    let mut hit = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let n = i as i64 + 1;
        let Some((_, _, made)) = indexed.iter().find(|(a, b, _)| *a <= n && n <= *b) else {
            continue;
        };
        let want = distinctive_tokens(line);
        // A line with nothing distinctive on it — a page number, a rule of
        // dashes — cannot be looked for and must not be counted against the
        // document. PDF exports are full of them.
        if want.is_empty() {
            hit += 1;
            continue;
        }
        let found = want.iter().filter(|t| made.contains(*t)).count();
        if found as f64 >= want.len() as f64 * LINE_TOKEN_RECALL {
            hit += 1;
        }
    }
    hit as f64 / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: &str = "\
### Writing the ISO

Unmount the device first.

    umount /dev/sdX*
    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress

Use the whole device (/dev/sdX), never a partition, and pass --dry-run first.";

    #[test]
    fn fenced_code_inline_code_and_paths_are_all_literals() {
        let chunk = "Run this:\n\n```bash\ndd if=x.iso of=/dev/sdX\n```\n\nCheck `/etc/fstab` and pass --dry-run.";
        let lits = extract_literals(chunk);
        assert!(lits.iter().any(|l| l.contains("dd if=x.iso")));
        assert!(lits.iter().any(|l| l == "/etc/fstab"));
        assert!(lits.iter().any(|l| l == "--dry-run"));
    }

    #[test]
    fn a_command_invented_in_a_caveat_is_caught() {
        // A caveat is prose the model wrote, and it is exactly where an
        // invented "run `wipefs --all` first" would appear. The literal check
        // has to reach it, or caveats become the one part of an artifact that
        // nothing verifies.
        let missing = missing_literals(
            "Write the image with `dd if=archlinux.iso of=/dev/sdX`.",
            &["First run `wipefs --all /dev/sdX`.".to_string()],
            WINDOW,
        );
        assert_eq!(missing, vec!["wipefs --all /dev/sdX".to_string()]);
    }

    #[test]
    fn a_caveat_quoting_a_real_command_is_not_flagged() {
        assert!(
            missing_literals(
                "Write the image to the device.",
                &[
                    "Unmount first: `dd if=archlinux.iso of=/dev/sdX` needs the whole device."
                        .to_string()
                ],
                WINDOW,
            )
            .is_empty()
        );
    }

    #[test]
    fn a_verbatim_chunk_reports_nothing_missing() {
        let chunk = "Unmount first.\n\n```bash\ndd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress\n```\n\nUse /dev/sdX with --dry-run.";
        assert!(missing_literals(chunk, &[], WINDOW).is_empty());
    }

    #[test]
    fn a_dropped_flag_is_reported() {
        // The model rewrote the command and lost oflag=sync. This is the
        // failure the whole check exists for.
        let chunk = "```bash\ndd if=archlinux.iso of=/dev/sdX bs=4M status=progress\n```";
        let missing = missing_literals(chunk, &[], WINDOW);
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("status=progress"));
    }

    #[test]
    fn indentation_and_whitespace_runs_do_not_count_as_a_mismatch() {
        // The window indents the command by four spaces; the chunk fences it.
        let chunk = "```\numount   /dev/sdX*\n```";
        assert!(missing_literals(chunk, &[], WINDOW).is_empty());
    }

    #[test]
    fn an_indented_code_block_counts_as_code() {
        // Reference documentation indents commands as often as it fences them.
        let chunk = "Write it:\n\n    dd if=archlinux.iso of=/dev/sdX bs=4M status=progress\n";
        let missing = missing_literals(chunk, &[], WINDOW);
        assert_eq!(missing.len(), 1, "the rewritten command must be caught");
    }

    #[test]
    fn a_slash_between_two_words_is_prose_not_a_path() {
        // Real capture: "enables/disables Nextcloud maintenance mode" was
        // flagged as a missing path, which is the kind of noise that trains a
        // reader to ignore the warning.
        let chunk = "Function that enables/disables maintenance mode and starts/stops nginx.";
        assert!(extract_literals(chunk).is_empty());
        // Genuine relative paths still count.
        let real = "See src/web/ui.rs and config/app.toml for the wiring.";
        let lits = extract_literals(real);
        assert!(lits.iter().any(|l| l == "src/web/ui.rs"));
        assert!(lits.iter().any(|l| l == "config/app.toml"));
    }

    #[test]
    fn prose_alone_has_no_literals_to_check() {
        assert!(extract_literals("Just some ordinary prose about disks.").is_empty());
    }

    #[test]
    fn a_span_over_the_lines_the_chunk_rewrote_is_plausible() {
        let claimed = "    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress";
        let chunk = "Write the image:\n\n```\ndd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress\n```";
        assert!(span_is_plausible(chunk, claimed));
    }

    #[test]
    fn a_span_pointing_at_unrelated_lines_is_not_plausible() {
        let claimed = "The kernel keeps a page cache of recently read blocks.";
        let chunk = "```\nmkfs.ext4 /dev/sdX1\n```\nFormat the partition with mkfs.";
        assert!(!span_is_plausible(chunk, claimed));
    }

    #[test]
    fn a_missing_span_is_recovered_from_the_lines_the_chunk_reproduced() {
        // The real synthesizer omits corpus_lines more often than not, and the
        // whole window is not a useful answer to "where did this come from".
        let chunk = "    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress";
        let found = locate_span(chunk, WINDOW, 101).expect("the command is in the window");
        // WINDOW line 6 (1-based) holds the dd command, so with the window
        // starting at 101 the span is line 106.
        assert_eq!(found, (106, 106));
    }

    /// A lecture handout, PDF-extracted: hard-wrapped at about eighty columns,
    /// so one sentence of prose is three lines of source.
    const WRAPPED: &str = "\
Die Verzeichniseinträge enthalten die Meta-Daten, wie Namen,
Dateigrößen, Attribute und Zeitstempel zu den gespeicherten
Dateien und Verzeichnissen.
Die Markierung End of File (EOF) zeigt das Dateiende an.";

    #[test]
    fn a_span_covers_every_line_a_reflowed_paragraph_came_from() {
        // Synthesis reflows: what the source wraps over three lines comes back
        // as one. Matching line against line then finds only the sentence that
        // happened to fit on one source line, so a chunk built from a whole
        // paragraph claimed a single line — and coverage, which counts the
        // lines some span names, read a fraction of the truth on every
        // hard-wrapped document.
        let chunk = "Die Verzeichniseinträge enthalten die Meta-Daten, wie Namen, Dateigrößen, Attribute und Zeitstempel zu den gespeicherten Dateien und Verzeichnissen.";
        assert_eq!(
            locate_span(chunk, WRAPPED, 1),
            Some((1, 3)),
            "the paragraph's own three source lines"
        );
    }

    #[test]
    fn a_reflowed_span_still_reaches_the_last_line_it_covers() {
        let chunk = "Die Verzeichniseinträge enthalten die Meta-Daten, wie Namen, Dateigrößen, Attribute und Zeitstempel zu den gespeicherten Dateien und Verzeichnissen.\n\nDie Markierung End of File (EOF) zeigt das Dateiende an.";
        assert_eq!(locate_span(chunk, WRAPPED, 101), Some((101, 104)));
    }

    #[test]
    fn a_span_cannot_be_located_for_a_chunk_that_shares_no_lines() {
        assert_eq!(
            locate_span("Something else entirely, rewritten freely.", WINDOW, 1),
            None
        );
    }

    #[test]
    fn coverage_counts_a_line_whose_content_reached_an_artifact() {
        let raw =
            "Mount the filesystem first.\n\nThe timeout is 30 seconds.\nUnrelated trailing note.";
        // One artifact carrying both subjects, rewritten rather than copied.
        let made = "Mount the filesystem before anything else. A timeout of 30 seconds applies.";
        let cov = content_coverage(raw, &[(1, 4, made.into())]);
        assert!((cov - 2.0 / 3.0).abs() < 1e-6, "{cov}");
    }

    #[test]
    fn a_segment_that_produced_nothing_is_uncovered() {
        // The case the number exists for: a segment the model refused leaves
        // its lines out of every artifact, and that must be visible.
        let raw = "alpha bravo charlie\ndelta echo foxtrot";
        assert_eq!(content_coverage(raw, &[]), 0.0);
        assert_eq!(
            content_coverage(raw, &[(1, 1, "alpha bravo charlie".into())]),
            0.5
        );
    }

    #[test]
    fn a_rewritten_line_still_counts() {
        // The failure this replaced: an artifact that reproduces a line's
        // subject and values in its own words scored zero, because no span
        // could be matched back to it.
        let raw = "Der Startcluster steht im Verzeichniseintrag.";
        let made = "Verzeichniseintrag: hier steht der Startcluster der Datei.";
        assert_eq!(content_coverage(raw, &[(1, 1, made.into())]), 1.0);
    }

    #[test]
    fn a_line_with_nothing_distinctive_on_it_is_not_held_against_the_document() {
        // A page number from a PDF export. Nothing can be looked for, so
        // counting it lost would make every handout read as half-dropped.
        let raw = "32";
        assert_eq!(content_coverage(raw, &[(1, 1, String::new())]), 1.0);
    }
}
