//! Retrieval evaluation: the arithmetic over ranks, and the instruments that
//! read what use left behind.
//!
//! Ranking has several knobs — fusion, the per-source cap, recency weight,
//! reranking, priming — and hand-testing cannot judge them, because the
//! queries anyone thinks to type reuse words they remember from the passage
//! they are looking for. A knob change becomes a number that moved only over
//! searches somebody made in earnest and the verdicts they gave.
//!
//! Everything here reads the live base. `sweep` replays judged pairs and
//! observations under candidate settings; `lived` reads what a generation
//! earned while serving; `rehearsed` scores a candidate on the base's own
//! probes; `anchor` says whether the self-generated evidence still agrees
//! with the people using the base. The offline harness that once froze a
//! corpus to a file is gone: it opened nothing, so every knob that reads
//! engagement was a no-op there, priming included.
//!
//! `docs/evaluation.md` is the whole of it in prose.

pub mod anchor;
pub mod lived;
pub mod metrics;
pub mod rehearsed;
pub mod sweep;

/// The artifact ids that satisfy a grade naming `expected`: itself, plus
/// whatever superseded it.
///
/// A graded pair names the artifact that answered the query. When consolidation
/// merges it into another, or supersedes it in favour of one that plainly
/// replaced it, the knowledge is in the survivor and search returns that. The
/// grade is still satisfied; only the id changed.
///
/// Bounded rather than trusted to terminate. Chains should not exist — a merge
/// re-points what it hides, precisely so no reader lands on a hidden winner —
/// but a sweep that hangs on a cycle in the data is worse than one that stops
/// looking after a few hops.
pub async fn satisfied_by(core: &crate::core::Core, expected: &str) -> Vec<String> {
    let mut out = vec![expected.to_string()];
    let mut cursor = expected.to_string();
    for _ in 0..8 {
        match core.store.get_artifact(&cursor).await {
            Ok(c) => match c.superseded_by {
                Some(next) => {
                    out.push(next.clone());
                    cursor = next;
                }
                None => break,
            },
            Err(_) => break,
        }
    }
    out
}
