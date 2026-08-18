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

/// Punctuation that wraps a token without belonging to it.
///
/// `*` and `_` are here because they are markdown, not because they are
/// punctuation: emphasis is the one wrapper that also appears in the set below
/// that decides whether a slash-carrying token is machine-shaped. `**Win7/8/10:**`
/// therefore matched — the asterisks supplied the second machine character —
/// and a merge was told it had dropped a path that was never a path. Trimmed
/// rather than dropped from that set, so `**/etc/fstab**` is still checked, as
/// `/etc/fstab`.
const TRIM: [char; 10] = ['(', ')', ',', '.', ';', ':', '"', '\'', '*', '_'];

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
/// lines inside fenced code blocks, indented lines, inline code spans, and bare
/// path- or flag-shaped tokens in the prose.
pub fn extract_literals(artifact_text: &str) -> Vec<String> {
    extract(artifact_text, true)
}

/// The same, minus the rule that an indented line is code.
///
/// For a comparison whose other side is not the text this one was copied from —
/// see `missing_machine_literals`, the only caller. A four-space indent is the
/// weakest of these signals by far: markdown nests a bullet list that way, so an
/// ordinary sentence one level deep is picked up whole and then required to
/// survive verbatim. What is left all says machine-shaped in the text itself —
/// a fence, backticks, a leading slash — and needs no guess from the layout.
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
        // Fenced blocks, and the indented kind markdown also treats as code —
        // reference documentation is full of the latter, and a command that
        // arrives indented rather than fenced still has to be verbatim.
        if fenced || (indented_is_code && (line.starts_with("    ") || line.starts_with('\t'))) {
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
    absent(extract_literals, artifact_text, caveats, segment_text)
}

/// Literals present in the artifact and absent from a text that is *not* its
/// source — merged text written over it, where only the machine-shaped strings
/// were ever meant to come through unchanged.
///
/// `missing_literals` asks whether a freshly written artifact copied its
/// commands out of the window it was extracted from. That is a fair question:
/// same text, same language, and the synthesizer was told to copy them. Merged
/// text is a rewrite by construction, and in this corpus routinely a rewrite
/// across German and English, so "did this sentence survive verbatim" has no
/// answer there. Asking it anyway is what vetoed a correct merge of two OneDrive
/// artifacts: three four-space-indented bullets, ordinary English prose with
/// their own descriptions, were extracted whole as literals and could not
/// possibly reappear word for word.
///
/// What stays is the part that does carry over: a fenced command, a backticked
/// registry key, a bare path. A merge that drops `/tmp/image.vdi` — the whole
/// point of the artifact it came from — is still refused.
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
            // Report what was actually looked for and not found, so the note on
            // the artifact names the command rather than the model's framing.
            Some(bare) => bare.to_string(),
            None => lit,
        })
        .collect()
}

/// A literal with a label the model put in front of it removed.
///
/// `Binär: 0010 1001 1111 1001`, for a source that says
/// `wird binär 0010 1001 1111 1001`, invents nothing: the digits — the part
/// that matters if somebody retypes them — are verbatim, and the label is
/// presentation. Reporting it as a possibly-invented command buries the real
/// misses under the model's formatting habits.
///
/// A colon *and a space*, and one word before it. That is what separates a
/// label from a command's own punctuation: `backup:/etc/fstab` and
/// `8080:8080` close up, `Binär: …` and `Run: …` do not. Stripping the first
/// kind would let a rewritten hostname pass as verbatim, which is the failure
/// this whole check exists to catch.
fn without_label(lit: &str) -> Option<&str> {
    let (label, rest) = lit.split_once(": ")?;
    let rest = rest.trim();
    if rest.is_empty() || label.split_whitespace().count() != 1 {
        return None;
    }
    Some(rest)
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
    let lines = line_coverage(raw_text, segments);
    if lines.is_empty() {
        return 0.0;
    }
    lines.iter().filter(|(_, ok)| *ok).count() as f64 / lines.len() as f64
}

/// Which non-empty lines survived, in order, as `(line number, covered)`.
///
/// The pass behind `content_coverage`. It asks whether a line's *wording*
/// survived the rewrite, which is a different question from whether any
/// artifact claims the line — the corpus page asks that one, off the spans,
/// and marks its answer in the source itself rather than as a number.
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
            // A line with nothing distinctive on it — a page number, a rule of
            // dashes — cannot be looked for and must not be counted against the
            // document. PDF exports are full of them.
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
    fn a_label_the_model_added_is_not_a_missing_literal() {
        // Line 635 of a real source read "wird binär 0010 1001 1111 1001". The
        // model fenced the digits and wrote "Binär:" in front of them, and the
        // check reported an invented command — a review task about formatting,
        // filed among the ones that matter.
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
        // `backup:/etc/fstab` must not be reduced to `/etc/fstab`, or a
        // rewritten hostname would stop being a missing literal. A label is
        // written with a space after the colon; a host and a port are not.
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
    fn only_the_merge_side_stops_reading_an_indent_as_code() {
        // Two questions that look like one. Against the source window an
        // indented line really is code often enough to be worth the false
        // positives: the artifact was copied out of that same text, so demanding
        // it verbatim costs nothing when it is prose. Against merged text — a
        // rewrite by construction, here across two languages — the same demand
        // vetoes correct merges, and a nested markdown bullet is prose far more
        // often than it is a command.
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

        // And what says machine-shaped in the text itself survives the
        // narrowing, indented or not.
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
        // A slash alone is not enough to call a token machine-shaped — that is
        // what keeps "enables/disables" out — so something else in the token has
        // to look like a path. Bold supplied it: the `*` in `**Win7/8/10:**` is
        // in that set, and the merge of three USB artifacts was refused for
        // dropping a "path" that is a Windows version list in bold.
        assert!(!looks_like_a_path_or_flag("**Win7/8/10:**"));
        assert!(!looks_like_a_path_or_flag("Win7/8/10"));
        assert!(!looks_like_a_path_or_flag("__enables/disables__"));

        // Emphasis around a real path is stripped, not disqualifying, and the
        // literal reported is the path itself so the merge is checked for what
        // it actually has to keep.
        assert!(looks_like_a_path_or_flag("**/etc/fstab**"));
        assert_eq!(
            extract_machine_literals("It lives in **/etc/fstab** on boot."),
            vec!["/etc/fstab".to_string()]
        );
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
