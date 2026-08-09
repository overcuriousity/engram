//! Does the chunk still say what the source said?
//!
//! The chunker is instructed to reproduce commands, paths and error strings
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
pub fn extract_literals(chunk_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in chunk_text.lines() {
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

/// Literals present in the chunk and absent from the window it came from.
pub fn missing_literals(chunk_text: &str, window_text: &str) -> Vec<String> {
    let haystack = normalize(window_text);
    extract_literals(chunk_text)
        .into_iter()
        .filter(|lit| !haystack.contains(&normalize(lit)))
        .collect()
}

/// Where in the window a chunk's own lines actually appear.
///
/// The chunker is asked for `source_lines` and frequently omits them. Falling
/// back to the whole window is honest but useless: the detail pane then marks
/// every line as the span, which points at nothing. Matching the chunk's lines
/// against the window recovers a real span for anything the model reproduced
/// verbatim, which is precisely the commands and paths worth pointing at.
///
/// Returns `None` when nothing matches, and the caller keeps the window.
pub fn locate_span(chunk_text: &str, window_body: &str, window_start: i64) -> Option<(i64, i64)> {
    let window: Vec<String> = window_body.lines().map(normalize).collect();
    let needles: Vec<String> = chunk_text
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
        if let Some(at) = window
            .iter()
            .position(|w| w == needle || w.contains(needle))
        {
            first = first.min(at);
            last = last.max(at);
        }
    }
    if first == usize::MAX {
        return None;
    }
    Some((window_start + first as i64, window_start + last as i64))
}

/// A chunk whose span points somewhere else entirely.
pub const FLAG_SPAN: &str = "span_unverified";

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
/// The chunker rewrites prose, so this cannot demand equality — only that a
/// third of the chunk's distinctive tokens appear in the claimed range. That is
/// enough to catch a span pointing at a different section, which is the failure
/// the detail pane would otherwise render as a rendering bug.
pub fn span_is_plausible(chunk_text: &str, claimed_text: &str) -> bool {
    let chunk = distinctive_tokens(chunk_text);
    if chunk.is_empty() {
        return true;
    }
    let claimed = distinctive_tokens(claimed_text);
    let shared = chunk.iter().filter(|t| claimed.contains(*t)).count();
    shared * 3 >= chunk.len()
}

/// Fraction of the source's non-blank lines that some chunk span covers.
pub fn coverage(spans: &[(i64, i64)], raw_text: &str) -> f64 {
    let lines: Vec<&str> = raw_text.lines().collect();
    let total = lines.iter().filter(|l| !l.trim().is_empty()).count();
    if total == 0 {
        return 0.0;
    }
    let mut covered = vec![false; lines.len()];
    for (start, end) in spans {
        let first = (*start).max(1);
        let last = (*end).min(lines.len() as i64);
        for n in first..=last {
            covered[(n - 1) as usize] = true;
        }
    }
    let hit = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| !l.trim().is_empty() && covered[*i])
        .count();
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
    fn a_verbatim_chunk_reports_nothing_missing() {
        let chunk = "Unmount first.\n\n```bash\ndd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress\n```\n\nUse /dev/sdX with --dry-run.";
        assert!(missing_literals(chunk, WINDOW).is_empty());
    }

    #[test]
    fn a_dropped_flag_is_reported() {
        // The model rewrote the command and lost oflag=sync. This is the
        // failure the whole check exists for.
        let chunk = "```bash\ndd if=archlinux.iso of=/dev/sdX bs=4M status=progress\n```";
        let missing = missing_literals(chunk, WINDOW);
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("status=progress"));
    }

    #[test]
    fn indentation_and_whitespace_runs_do_not_count_as_a_mismatch() {
        // The window indents the command by four spaces; the chunk fences it.
        let chunk = "```\numount   /dev/sdX*\n```";
        assert!(missing_literals(chunk, WINDOW).is_empty());
    }

    #[test]
    fn an_indented_code_block_counts_as_code() {
        // Reference documentation indents commands as often as it fences them.
        let chunk = "Write it:\n\n    dd if=archlinux.iso of=/dev/sdX bs=4M status=progress\n";
        let missing = missing_literals(chunk, WINDOW);
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
        // The real chunker omits source_lines more often than not, and the
        // whole window is not a useful answer to "where did this come from".
        let chunk = "    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress";
        let found = locate_span(chunk, WINDOW, 101).expect("the command is in the window");
        // WINDOW line 6 (1-based) holds the dd command, so with the window
        // starting at 101 the span is line 106.
        assert_eq!(found, (106, 106));
    }

    #[test]
    fn a_span_cannot_be_located_for_a_chunk_that_shares_no_lines() {
        assert_eq!(
            locate_span("Something else entirely, rewritten freely.", WINDOW, 1),
            None
        );
    }

    #[test]
    fn coverage_is_the_fraction_of_non_blank_lines_claimed() {
        let raw = "one\n\ntwo\nthree\nfour"; // four non-blank lines
        assert!((coverage(&[(1, 1), (3, 3)], raw) - 0.5).abs() < 1e-6);
        assert!((coverage(&[(1, 5)], raw) - 1.0).abs() < 1e-6);
        assert_eq!(coverage(&[], raw), 0.0);
        // Overlapping spans must not push coverage above one.
        assert!((coverage(&[(1, 5), (2, 4)], raw) - 1.0).abs() < 1e-6);
    }
}
