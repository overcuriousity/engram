//! Retrieval evaluation over the live base: rank arithmetic, and instruments that read what use
//! left behind. `docs/evaluation.md` is the whole of it in prose.

pub mod anchor;
pub mod lived;
pub mod metrics;
pub mod rehearsed;
pub mod sweep;

/// The artifact ids that satisfy a grade naming `expected`: itself plus whatever superseded it.
/// Bounded in hops so a cycle in the data cannot hang a sweep.
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
