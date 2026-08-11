pub const FLAG_LITERALS: &str = "literals_unverified";

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
    t.contains('/') && t.contains(['.', '-', '_', '=', '$', '*'])
}

pub fn extract_literals(artifact_text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in artifact_text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced || line.starts_with("    ") || line.starts_with('\t') {
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
            Some(bare) => bare.to_string(),
            None => lit,
        })
        .collect()
}

fn without_label(lit: &str) -> Option<&str> {
    let (label, rest) = lit.split_once(": ")?;
    let rest = rest.trim();
    if rest.is_empty() || label.split_whitespace().count() != 1 {
        return None;
    }
    Some(rest)
}

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
            if !text.is_empty() && !n.is_empty() {
                text.push(' ');
            }
            starts.push(text.len());
            text.push_str(&n);
        }
        Self { text, starts }
    }

    fn line_at(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
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

pub const LOW_COVERAGE: f64 = 0.6;

fn distinctive_tokens(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '=')))
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| t.len() > 3)
        .collect()
}

pub fn span_is_plausible(artifact_text: &str, claimed_text: &str) -> bool {
    let chunk = distinctive_tokens(artifact_text);
    if chunk.is_empty() {
        return true;
    }
    let claimed = distinctive_tokens(claimed_text);
    let shared = chunk.iter().filter(|t| claimed.contains(*t)).count();
    shared * 3 >= chunk.len()
}

const LINE_TOKEN_RECALL: f64 = 0.5;

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
        let missing = missing_literals(
            "Write the image with `dd if=archlinux.iso of=/dev/sdX`.",
            &["First run `wipefs --all /dev/sdX`.".to_string()],
            WINDOW,
        );
        assert_eq!(missing, vec!["wipefs --all /dev/sdX".to_string()]);
    }

    #[test]
    fn a_label_the_model_added_is_not_a_missing_literal() {
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
        let chunk = "    dd if=archlinux.iso of=/dev/sdX bs=4M oflag=sync status=progress";
        let found = locate_span(chunk, WINDOW, 101).expect("the command is in the window");
        assert_eq!(found, (106, 106));
    }

    const WRAPPED: &str = "\
Die Verzeichniseinträge enthalten die Meta-Daten, wie Namen,
Dateigrößen, Attribute und Zeitstempel zu den gespeicherten
Dateien und Verzeichnissen.
Die Markierung End of File (EOF) zeigt das Dateiende an.";

    #[test]
    fn a_span_covers_every_line_a_reflowed_paragraph_came_from() {
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
        let raw = "alpha bravo charlie\ndelta echo foxtrot";
        assert_eq!(content_coverage(raw, &[]), 0.0);
        assert_eq!(
            content_coverage(raw, &[(1, 1, "alpha bravo charlie".into())]),
            0.5
        );
    }

    #[test]
    fn a_rewritten_line_still_counts() {
        let raw = "Der Startcluster steht im Verzeichniseintrag.";
        let made = "Verzeichniseintrag: hier steht der Startcluster der Datei.";
        assert_eq!(content_coverage(raw, &[(1, 1, made.into())]), 1.0);
    }

    #[test]
    fn a_line_with_nothing_distinctive_on_it_is_not_held_against_the_document() {
        let raw = "32";
        assert_eq!(content_coverage(raw, &[(1, 1, String::new())]), 1.0);
    }
}
