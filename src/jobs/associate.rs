//! Learning what belongs together, and saying so.
//!
//! Two things happen here and they are deliberately not the same job. The sweep
//! is pure SQLite: it replays the search log, strengthens the pairs that were
//! reached together, fades and prunes the ones that were not, and decides which
//! links are worth asking about. The judge is one model call on one link, armed
//! by the sweep and paced by the queue like every other call in the system.

use crate::core::Core;
use crate::error::Result;

/// One sweep over everything learned since the last one.
pub async fn run(core: &Core) -> Result<()> {
    if !core.associate.enabled || !core.feedback.enabled {
        return Ok(());
    }
    Ok(())
}

/// One link, one call. `target` is `"<a_id>|<b_id>"`.
pub async fn judge(core: &Core, target: &str) -> Result<()> {
    let _ = (core, target);
    Ok(())
}
