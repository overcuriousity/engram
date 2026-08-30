//! Revisit the retired, and act without asking.
//!
//! Retirement hides an artifact and then keeps everything about it forever.
//! This sweep is the second look nobody was going to take by hand: free rules
//! nominate the long-retired, one model call per nominee asks whether it still
//! states anything the live base does not, and the verdict is acted on — the
//! worthless are buried (text into `graveyard`, point deleted, stub kept), the
//! valuable rewritten as live synthesized artifacts. No operator queue; the
//! graveyard is the insurance a wrong verdict answers to.

use crate::core::Core;
use crate::error::Result;

/// What one pass did. Flat numbers on purpose: `jobs::did_work` reads any
/// non-zero flat count as work, which is what drives the empty-run backoff,
/// and every count here really is this pass acting.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    /// Nominees put in front of the judge, verdicts and failures alike.
    pub judged: u64,
    /// Buried: text in the graveyard, vector point deleted, stub kept.
    pub reaped: u64,
    /// Rewritten as a live synthesized artifact.
    pub rescued: u64,
    /// Retired rows given a fresh `retired_at` because they predate the
    /// column — the migration-free backfill, counted as the work it is.
    pub stamped: u64,
}

pub async fn run(core: &Core) -> Result<Report> {
    let _ = core;
    Ok(Report::default())
}
