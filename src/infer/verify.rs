//! Does the chunk still say what the source said? A paraphrased command later gets pasted into a root shell.

pub const FLAG_LITERALS: &str = "literals_unverified";

/// Collapses whitespace runs; a changed flag or renamed path still differs.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `*` and `_` are markdown emphasis, trimmed so `**Win7/8/10:**` is not taken for a path but `**/etc/fstab**` still is.
const TRIM: [char; 10] = ['(', ')', ',', '.', ';', ':', '"', '\'', '*', '_'];

fn looks_like_a_path_or_flag(token: &str) -> bool {
    let t = token.trim_matches(|c: char| TRIM.contains(&c));
    if t.len() < 3 {
        return false;
    }
    if t.starts_with("--") || t.starts_with('/') || t.starts_with("~/") || t.starts_with("./") {
        return true;
    }
    // A slash alone is not a path ("and/or"); a relative path carries something else machine-shaped too.
    t.contains('/') && t.contains(['.', '-', '_', '=', '$', '*'])
}

/// Fenced and indented code lines, inline code spans, and bare path- or flag-shaped tokens.
pub fn extract_literals(artifact_text: &str) -> Vec<String> {
    extract(artifact_text, true)
}

/// Without the indented-line rule: markdown nests bullets by four spaces. For `missing_machine_literals`.
pub fn extract_machine_literals(artifact_text: &str) -> Vec<String> {
    extract(artifact_text, false)
}

fn extract(artifact_text: &str, indented_is_code: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in artifact_text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        // Indented code counts: reference documentation indents commands as often as it fences them.
        if fenced || (indented_is_code && (line.starts_with("    ") || line.starts_with('\t'))) {
            if !line.trim().is_empty() {
                out.push(line.trim().to_string());
            }
            continue;
        }

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

/// Caveats get the same check: a caveat saying to run something first is a command too.
pub fn missing_literals(
    artifact_text: &str,
    caveats: &[String],
    segment_text: &str,
) -> Vec<String> {
    absent(extract_literals, artifact_text, caveats, segment_text)
}

/// Against merged text, which is a rewrite (often across languages): only machine-shaped strings must carry over.
pub fn missing_machine_literals(
    artifact_text: &str,
    caveats: &[String],
    merged_text: &str,
) -> Vec<String> {
    absent(
        extract_machine_literals,
        artifact_text,
        caveats,
        merged_text,
    )
}

/// Digit runs with what a reader would retype (`1.22.0`, `3,5%`), not every token with a digit like `SHA256`.
pub fn extract_numbers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        let t = token.trim_matches(|c: char| TRIM.contains(&c));
        if t.chars().any(|c| c.is_ascii_digit())
            && t.chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | ':' | '-' | '/' | '%'))
        {
            out.push(t.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// For `condense`: a dropped version in prose changes meaning silently. Not asked of a merge.
pub fn missing_numbers(artifact_text: &str, caveats: &[String], rewritten: &str) -> Vec<String> {
    absent(extract_numbers, artifact_text, caveats, rewritten)
}

fn absent(
    extract: fn(&str) -> Vec<String>,
    artifact_text: &str,
    caveats: &[String],
    haystack_text: &str,
) -> Vec<String> {
    let haystack = normalize(haystack_text);
    let mut all = extract(artifact_text);
    for c in caveats {
        all.extend(extract(c));
    }
    all.sort();
    all.dedup();
    all.into_iter()
        .filter(|lit| {
            if haystack.contains(&normalize(lit)) {
                return false;
            }
            match without_label(lit) {
                Some(bare) => !haystack.contains(&normalize(bare)),
                None => true,
            }
        })
        .map(|lit| match without_label(&lit) {
            // Report what was looked for, so the note names the command rather than the model's framing.
            Some(bare) => bare.to_string(),
            None => lit,
        })
        .collect()
}

/// A colon and a space after one word is a label (`Binär: …`); `backup:/etc/fstab` and `8080:8080` are not.
fn without_label(lit: &str) -> Option<&str> {
    let (label, rest) = lit.split_once(": ")?;
    let rest = rest.trim();
    if rest.is_empty() || label.split_whitespace().count() != 1 {
        return None;
    }
    Some(rest)
}

/// Recovers a span for verbatim lines when the model omits `corpus_lines`; `None` keeps the window.
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

/// The window as one string, so a sentence hard-wrapped in the source matches its reflowed chunk.
struct Flattened {
    text: String,
    starts: Vec<usize>,
}

impl Flattened {
    fn of(segment_body: &str) -> Self {
        let mut text = String::new();
        let mut starts = Vec::new();
        for line in segment_body.lines() {
            let n = normalize(line);
            // The separator stands in for the source's line break.
            if !text.is_empty() && !n.is_empty() {
                text.push(' ');
            }
            starts.push(text.len());
            text.push_str(&n);
        }
        Self { text, starts }
    }

    /// Blank lines share their neighbour's offset and never win: a match cannot begin inside one.
    fn line_at(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            // Several lines can share a start offset; the last one holds the text.
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

/// Below this the segmenter probably dropped part of the document.
pub const LOW_COVERAGE: f64 = 0.6;

fn distinctive_tokens(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '=')))
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| t.len() > 3)
        .collect()
}

/// Half, because synthesis rewrites: a line survives as its subject and values, not its wording.
const LINE_TOKEN_RECALL: f64 = 0.5;

/// Whether the content arrived, not whether a span claims the line; lines outside every segment are uncovered.
pub fn content_coverage(raw_text: &str, segments: &[(i64, i64, String)]) -> f64 {
    let lines = line_coverage(raw_text, segments);
    if lines.is_empty() {
        return 0.0;
    }
    lines.iter().filter(|(_, ok)| *ok).count() as f64 / lines.len() as f64
}

/// `(line number, covered)` by wording; the corpus page asks the span question separately.
fn line_coverage(raw_text: &str, segments: &[(i64, i64, String)]) -> Vec<(i64, bool)> {
    let indexed: Vec<(i64, i64, std::collections::HashSet<String>)> = segments
        .iter()
        .map(|(a, b, text)| (*a, *b, distinctive_tokens(text)))
        .collect();

    raw_text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            let n = i as i64 + 1;
            let Some((_, _, made)) = indexed.iter().find(|(a, b, _)| *a <= n && n <= *b) else {
                return (n, false);
            };
            let want = distinctive_tokens(line);
            // A page number or rule of dashes cannot be looked for and must not count against the document.
            if want.is_empty() {
                return (n, true);
            }
            let found = want.iter().filter(|t| made.contains(*t)).count();
            (n, found as f64 >= want.len() as f64 * LINE_TOKEN_RECALL)
        })
        .collect()
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
        // Caveats must be checked, or they become the one unverified part of an artifact.
        let missing = missing_literals(
            "Write the image with `dd if=archlinux.iso of=/dev/sdX`.",
            &["First run `wipefs --all /dev/sdX`.".to_string()],
            WINDOW,
        );
        assert_eq!(missing, vec!["wipefs --all /dev/sdX".to_string()]);
    }

    #[test]
    fn a_label_the_model_added_is_not_a_missing_literal() {
        // From a real source: a fenced value with a label in front invents nothing.
        let window = "Die Zahl 29 F9 wird binär 0010 1001 1111 1001 gespeichert.";
        let chunk = "```\nBinär: 0010 1001 1111 1001\n```";
        assert!(missing_literals(chunk, &[], window).is_empty());
    }

    #[test]
    fn an_invented_command_is_still_caught_when_it_carries_a_label() {
        let window = "Unmount the device first.";
        let chunk = "```\nRun: wipefs --all /dev/sdX\n```";
        assert_eq!(
            missing_literals(chunk, &[], window),
            vec!["wipefs --all /dev/sdX".to_string()]
        );
    }

    #[test]
    fn a_command_whose_own_colon_looks_like_a_label_is_not_weakened() {
        // A label has a space after the colon; a host and a port do not.
        let window = "Copy /etc/fstab from the box.";
        let chunk = "```\nbackup:/etc/fstab\n```";
        assert_eq!(
            missing_literals(chunk, &[], window),
            vec!["backup:/etc/fstab".to_string()]
        );
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
        let chunk = "```bash\ndd if=archlinux.iso of=/dev/sdX bs=4M status=progress\n```";
        let missing = missing_literals(chunk, &[], WINDOW);
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("status=progress"));
    }

    #[test]
    fn indentation_and_whitespace_runs_do_not_count_as_a_mismatch() {
        let chunk = "```\numount   /dev/sdX*\n```";
        assert!(missing_literals(chunk, &[], WINDOW).is_empty());
    }

    #[test]
    fn an_indented_code_block_counts_as_code() {
        let chunk = "Write it:\n\n    dd if=archlinux.iso of=/dev/sdX bs=4M status=progress\n";
        let missing = missing_literals(chunk, &[], WINDOW);
        assert_eq!(missing.len(), 1, "the rewritten command must be caught");
    }

    #[test]
    fn a_slash_between_two_words_is_prose_not_a_path() {
        // "enables/disables" flagged as a path is noise that trains readers to ignore the warning.
        let chunk = "Function that enables/disables maintenance mode and starts/stops nginx.";
        assert!(extract_literals(chunk).is_empty());
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
    fn only_the_merge_side_stops_reading_an_indent_as_code() {
        // Against merged text an indented line is far more often a nested bullet than a command.
        let nested = "* Personal directory contents:\n    * SyncEngine.odl: Logs of \
                      synchronized files and file hashes.";

        assert_eq!(
            extract_literals(nested),
            vec!["* SyncEngine.odl: Logs of synchronized files and file hashes.".to_string()],
            "the source-window check must keep reading an indent as code"
        );
        assert!(
            extract_machine_literals(nested).is_empty(),
            "an indented sentence is still being required to survive a merge: {:?}",
            extract_machine_literals(nested)
        );

        let both = "Run it:\n    `xmount --in ewf --out dd`\nleaves /tmp/image.vdi behind.";
        for lit in ["xmount --in ewf --out dd", "/tmp/image.vdi"] {
            assert!(
                extract_machine_literals(both).iter().any(|l| l == lit),
                "{lit} stopped being checked: {:?}",
                extract_machine_literals(both)
            );
        }
    }

    #[test]
    fn markdown_emphasis_does_not_make_a_word_into_a_path() {
        // Bold supplied the second machine character and a merge was refused over a version list.
        assert!(!looks_like_a_path_or_flag("**Win7/8/10:**"));
        assert!(!looks_like_a_path_or_flag("Win7/8/10"));
        assert!(!looks_like_a_path_or_flag("__enables/disables__"));

        assert!(looks_like_a_path_or_flag("**/etc/fstab**"));
        assert_eq!(
            extract_machine_literals("It lives in **/etc/fstab** on boot."),
            vec!["/etc/fstab".to_string()]
        );
    }

    #[test]
    fn a_missing_span_is_recovered_from_the_lines_the_chunk_reproduced() {
        let chunk = "    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress";
        let found = locate_span(chunk, WINDOW, 101).expect("the command is in the window");
        // WINDOW line 6 (1-based) holds the dd command, so with the window starting at 101 the span is line 106.
        assert_eq!(found, (106, 106));
    }

    const WRAPPED: &str = "\
Die Verzeichniseinträge enthalten die Meta-Daten, wie Namen,
Dateigrößen, Attribute und Zeitstempel zu den gespeicherten
Dateien und Verzeichnissen.
Die Markierung End of File (EOF) zeigt das Dateiende an.";

    #[test]
    fn a_span_covers_every_line_a_reflowed_paragraph_came_from() {
        // The source wraps over three lines what the chunk reflows into one.
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
        let made = "Mount the filesystem before anything else. A timeout of 30 seconds applies.";
        let cov = content_coverage(raw, &[(1, 4, made.into())]);
        assert!((cov - 2.0 / 3.0).abs() < 1e-6, "{cov}");
    }

    #[test]
    fn a_segment_that_produced_nothing_is_uncovered() {
        // A segment the model refused leaves its lines out of every artifact, and that must be visible.
        let raw = "alpha bravo charlie\ndelta echo foxtrot";
        assert_eq!(content_coverage(raw, &[]), 0.0);
        assert_eq!(
            content_coverage(raw, &[(1, 1, "alpha bravo charlie".into())]),
            0.5
        );
    }

    #[test]
    fn a_rewritten_line_still_counts() {
        // A line restated in other words must still count as covered.
        let raw = "Der Startcluster steht im Verzeichniseintrag.";
        let made = "Verzeichniseintrag: hier steht der Startcluster der Datei.";
        assert_eq!(content_coverage(raw, &[(1, 1, made.into())]), 1.0);
    }

    #[test]
    fn a_line_with_nothing_distinctive_on_it_is_not_held_against_the_document() {
        // Counting page numbers as lost would make every handout read as half-dropped.
        let raw = "32";
        assert_eq!(content_coverage(raw, &[(1, 1, String::new())]), 1.0);
    }
}
