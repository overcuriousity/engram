//! Writing, verifying and undoing a merge.
//!
//! Merging is the one thing in this system that puts model-written text where
//! stored text used to be, so it is also the one thing that can lose knowledge
//! without anyone noticing: a plausible paragraph reads exactly as well without
//! the number it dropped, ranks exactly as well, and nothing downstream can
//! tell. `losses` is what stands between a verdict and a write.
//!
//! Both checks are local and free — two token sets and one substring pass — and
//! that is the point. The argument for letting a model rewrite stored knowledge
//! unattended is not that it rarely goes wrong; it is that when it does, a rule
//! that costs nothing catches it before anything is written.

use crate::infer::prompt::MergedDraft;
use crate::store::artifacts::Chunk;

/// Every value and literal in `roots` that `draft` does not carry.
///
/// Empty means the merge may be written. Anything else is a merge that would
/// have lost something, and the caller escalates rather than retrying: the text
/// is what was wrong, and a person can read what it would have cost.
///
/// Both halves search the draft's text *and* its caveats. A caveat is stored,
/// rendered and recoverable, so a value demoted there has not been lost — this
/// checks for loss, not for prominence. Deciding that a value belongs in the
/// caveats rather than the body is exactly the judgement a merge is for.
pub fn losses(roots: &[Chunk], draft: &MergedDraft) -> Vec<String> {
    let mut haystack = draft.text.clone();
    for c in &draft.caveats {
        haystack.push(' ');
        haystack.push_str(c);
    }

    let have = crate::infer::facts::fact_tokens(&haystack);
    let mut out: Vec<String> = Vec::new();

    for r in roots {
        // Values: a version, a timeout, a port. The failure this catches is a
        // model answering "duplicate" and then quietly picking a side while
        // writing, which is a conflict resolved by deletion.
        for tok in crate::infer::facts::fact_tokens(&r.text) {
            if !have.contains(&tok) {
                out.push(tok);
            }
        }
        // Literals: commands, paths, flags, error strings. The existing check,
        // with the merged text as the haystack instead of the segment —
        // `missing_literals(artifact_text, caveats, haystack)` asks which
        // literals of the first argument are absent from the third, which is
        // exactly this question with the arguments in this order.
        //
        // `verify`'s module header states the stake: a paraphrased command is a
        // command that later gets pasted into a root shell.
        out.extend(crate::infer::verify::missing_literals(
            &r.text, &r.caveats, &haystack,
        ));
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::{ArtifactStatus, EmbedState, Provenance};

    fn draft(text: &str) -> MergedDraft {
        MergedDraft {
            title: None,
            text: text.into(),
            category: None,
            tags: vec![],
            caveats: vec![],
        }
    }

    /// A captured artifact carrying only the text the checks read.
    fn root(text: &str) -> Chunk {
        Chunk {
            id: "root".into(),
            corpus_id: Some("corpus".into()),
            provenance: Provenance::Captured,
            source_count: 0,
            ordinal: 0,
            text: text.into(),
            corpus_span: None,
            title: None,
            category: None,
            tags: vec![],
            embed_state: EmbedState::Embedded,
            embed_model: None,
            created_at: 0,
            embed_rev: 0,
            segment_idx: None,
            flags: vec![],
            flag_detail: None,
            superseded_by: None,
            caveats: vec![],
            status: ArtifactStatus::Active,
            last_verified_at: None,
        }
    }

    #[test]
    fn a_merge_that_keeps_both_values_is_allowed() {
        let roots = [
            root("The request timeout is 30 seconds."),
            root("The request timeout is 90 seconds."),
        ];
        let d = draft(
            "Sources differ on the request timeout: an earlier capture gives 30 seconds, \
             a later one 90 seconds.",
        );
        assert!(losses(&roots, &d).is_empty(), "{:?}", losses(&roots, &d));
    }

    #[test]
    fn a_merge_that_drops_a_value_is_refused() {
        // The one way this feature can destroy knowledge without anyone
        // noticing: the model answers "duplicate" and quietly picks a side while
        // writing. The result reads well, ranks well, and the missing number is
        // gone from the base — a conflict resolved by deletion.
        let roots = [
            root("The request timeout is 30 seconds."),
            root("The request timeout is 90 seconds."),
        ];
        let d = draft("The request timeout is 90 seconds.");
        assert_eq!(losses(&roots, &d), vec!["30".to_string()]);
    }

    #[test]
    fn a_value_moved_into_the_caveats_is_not_lost() {
        // Caveats are stored and rendered beside the artifact, so a value
        // demoted there is still recoverable. This checks for loss, not for
        // prominence — deciding what belongs in the body is what a merge is for.
        let roots = [
            root("The request timeout is 30 seconds."),
            root("The request timeout is 90 seconds."),
        ];
        let mut d = draft("The request timeout is 90 seconds.");
        d.caveats = vec!["An earlier capture gave 30 seconds.".into()];
        assert!(losses(&roots, &d).is_empty(), "{:?}", losses(&roots, &d));
    }

    #[test]
    fn a_merge_that_paraphrases_a_command_is_refused() {
        // A paraphrased command is a command that later gets pasted into a root
        // shell. The literal check is the same one synthesis already runs, with
        // the merged text as the haystack instead of the source window.
        let roots = [
            root("Attach it with `mount --bind /src /dst`."),
            root("Bind mounts attach a directory elsewhere."),
        ];
        let d = draft("Bind mounts attach a directory elsewhere; use the bind mount option.");
        let lost = losses(&roots, &d);
        assert!(
            lost.iter().any(|l| l.contains("mount --bind")),
            "the literal check let a paraphrased command through: {lost:?}"
        );
    }

    #[test]
    fn a_merge_that_reproduces_the_command_verbatim_is_allowed() {
        let roots = [
            root("Attach it with `mount --bind /src /dst`."),
            root("Bind mounts attach a directory elsewhere."),
        ];
        let d = draft(
            "Bind mounts attach a directory elsewhere. Attach one with \
             `mount --bind /src /dst`.",
        );
        assert!(losses(&roots, &d).is_empty(), "{:?}", losses(&roots, &d));
    }

    #[test]
    fn a_literal_a_root_carried_only_in_its_caveats_still_has_to_survive() {
        // Caveats are the newest place model prose appears, and one that says to
        // run something first is a command like any other. `missing_literals`
        // already reads them on the source side; this pins that a merge cannot
        // drop them.
        let mut r = root("Mount the filesystem before writing.");
        r.caveats = vec!["Only after running `systemctl stop app`.".into()];
        let d = draft("Mount the filesystem before writing.");
        let lost = losses(&[r], &d);
        assert!(
            lost.iter().any(|l| l.contains("systemctl stop app")),
            "a caveat's command was dropped without complaint: {lost:?}"
        );
    }

    #[test]
    fn a_merge_of_three_roots_is_checked_against_all_of_them() {
        // The fan-in cap allows up to eight. A check that only read the first
        // two would pass a merge that dropped everything the third said.
        let roots = [
            root("Port 8080 is the default."),
            root("The timeout is 30s."),
            root("Retries are capped at 5."),
        ];
        let d = draft("Port 8080 is the default and the timeout is 30s.");
        assert_eq!(losses(&roots, &d), vec!["5".to_string()]);
    }
}
