//! `Stage::Decisions`: lapse unanswered rows, close outcome windows, apply waiting acts, seat waiting asks.

use crate::core::Core;
use crate::core::judging::settle_counted;
use crate::error::Result;
use crate::store::decision_log::State;

/// Waiting acts looked at per sweep, highest top first.
const ACTS_PER_RUN: i64 = 200;
/// An act nobody undid in this long is kept.
const OUTCOME_WINDOW_SECS: i64 = 30 * 86_400;

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub applied: u64,
    pub reasked: u64,
    pub seated: u64,
    pub lapsed: u64,
    pub kept: u64,
}

pub async fn run(core: &Core) -> Result<Report> {
    let now = crate::store::now();
    let mut r = Report {
        lapsed: core
            .store
            .lapse_asks(now - i64::from(core.decide_cfg.ask_lapse_days) * 86_400)
            .await?,
        kept: core
            .store
            .close_outcome_windows(now - OUTCOME_WINDOW_SECS)
            .await?,
        ..Default::default()
    };
    for d in core.store.waiting_acts(ACTS_PER_RUN).await? {
        let Some(applier) = core.appliers.get(&d.question_id) else {
            continue;
        };
        // One decision that fails is retried next sweep; it does not hold up the rest.
        match applier.unchanged(core, &d).await {
            Ok(true) => match settle_counted(core, d).await {
                Ok((_, true)) => r.applied += 1,
                Ok((_, false)) => {}
                Err(e) => tracing::warn!(error = %e, "a waiting decision failed to apply"),
            },
            Ok(false) => {
                // Closed before the reopen, so a second worker reaching it does neither.
                if !core
                    .store
                    .set_decision_state(&d.id, State::Waiting, State::Done)
                    .await?
                {
                    continue;
                }
                match applier.reopen(core, &d).await {
                    Ok(()) => {
                        core.store.set_decision_reason(&d.id, "changed").await?;
                        r.reasked += 1;
                    }
                    // Put back as it was, so the next sweep finds it changed and tries again.
                    Err(e) => {
                        tracing::warn!(decision = d.id, error = %e, "could not ask a changed subject again");
                        core.store
                            .set_decision_state(&d.id, State::Done, State::Waiting)
                            .await?;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(decision = d.id, error = %e, "could not read a decision's subjects")
            }
        }
    }
    let max = i64::try_from(core.decide_cfg.ask_open_max).unwrap_or(i64::MAX);
    let free = max - core.store.open_ask_count().await?;
    if free > 0 {
        for d in core.store.waiting_asks(free).await? {
            if core.store.seat_ask(&d.id, max).await? {
                r.seated += 1;
            }
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::jobs::Stage;

    #[tokio::test]
    async fn the_sweep_is_always_armed_every_tick() {
        let mut core = crate::core::test_support::test_core().await;
        core.consolidate.enabled = false;
        core.decide_cfg.tick_mins = 7;
        assert!(
            crate::core::background::periodic_units(&core)
                .iter()
                .any(|(s, _)| *s == Stage::Decisions)
        );
        assert_eq!(
            crate::core::background::periodic_period(&core, Stage::Decisions),
            Some(std::time::Duration::from_secs(7 * 60))
        );
    }

    #[tokio::test]
    async fn an_empty_base_reports_no_work() {
        let core = crate::core::test_support::test_core().await;
        let r = run(&core).await.unwrap();
        assert_eq!(
            (r.applied, r.reasked, r.seated, r.lapsed, r.kept),
            (0, 0, 0, 0, 0)
        );
    }
}
