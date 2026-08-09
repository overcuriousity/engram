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
    t.starts_with("--") || t.starts_with('/') || t.starts_with("~/") || t.contains('/')
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
    fn prose_alone_has_no_literals_to_check() {
        assert!(extract_literals("Just some ordinary prose about disks.").is_empty());
    }
}
