//! `Stage::Decisions`: lapse unanswered rows, close outcome windows, apply waiting acts, seat waiting asks,
//! while the base is quiet, queue readings for the notes that have none, and queue relevance labels
//! for recorded searches.

use crate::core::Core;
use crate::core::judging::settle_counted;
use crate::error::Result;
use crate::store::decision_log::State;
use crate::store::jobs::Stage;

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
    pub scanned: u64,
    pub labelled: u64,
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
    // A stage that died between `ask` and `settle`: closed and asked again, never applied.
    let orphaned = now - i64::from(core.decide_cfg.tick_mins.max(1)) * 60;
    for d in core.store.unsettled_acts(orphaned).await? {
        let Some(applier) = core.appliers.get(&d.question_id) else {
            continue;
        };
        if !core.store.close_unsettled(&d.id).await? {
            continue;
        }
        if let Err(e) = applier.reopen(core, &d).await {
            tracing::warn!(decision = d.id, error = %e, "could not ask an orphaned decision again");
            core.store
                .set_decision_state(&d.id, State::Done, State::Waiting)
                .await?;
            core.store.set_decision_reason(&d.id, "unsettled").await?;
            continue;
        }
        r.reasked += 1;
    }
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
                if !core.store.claim_act(&d.id).await? {
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
    r.scanned = scan(core).await?;
    r.labelled = label(core).await?;
    Ok(r)
}

/// Searches looked at per sweep for labelling.
const LABEL_LOOKAHEAD: i64 = 64;

/// Unlabelled searches, oldest first, in a quarter of the tick's budget, each costing its live
/// candidates. The first may overrun it when no label unit is in flight; one queued holds its
/// share, and the oldest that does not fit waits rather than being passed.
async fn label(core: &Core) -> Result<u64> {
    let mut left = core.judging_budget().await? / 4;
    if left == 0 {
        return Ok(0);
    }
    let version = crate::core::judging::catalogue::answers_query("").version;
    let (mut armed, mut held) = (0, false);
    for id in core
        .store
        .unlabelled_searches(version, core.decider.model(), LABEL_LOOKAHEAD)
        .await?
    {
        let Some((_, candidates)) = core.store.search_to_label(&id).await? else {
            continue;
        };
        let live = core
            .store
            .artifacts_by_ids(&candidates)
            .await?
            .iter()
            .filter(|c| c.in_results())
            .count();
        let cost = live.max(1);
        if cost > left && (armed > 0 || held) {
            break;
        }
        left = left.saturating_sub(cost);
        if core.store.live_job(Stage::Label, &id).await? {
            held = true;
            continue;
        }
        crate::jobs::label::arm(core, &id).await?;
        armed += 1;
    }
    Ok(armed)
}

/// Notes from before readings existed, newest first, while nobody is using the base. Half of what
/// the tick has left, so the pair judge is never starved; a note already queued keeps its slot, so
/// a queue that has not caught up is not piled onto.
async fn scan(core: &Core) -> Result<u64> {
    let idle = crate::store::now() - core.evolve.idle_secs.max(0);
    if core.store.activity_since(idle).await? {
        return Ok(0);
    }
    let half = i64::try_from(core.judging_budget().await? / 2).unwrap_or(i64::MAX);
    if half == 0 {
        return Ok(0);
    }
    let asked = crate::jobs::read::reading_questions();
    let versions: Vec<(&str, u32)> = asked.iter().map(|a| (a.id, a.version)).collect();
    let mut armed = 0;
    for id in core
        .store
        .artifacts_lacking_readings(&versions, half)
        .await?
    {
        if core.store.live_job(Stage::Read, &id).await? {
            continue;
        }
        crate::jobs::read::arm(core, &id).await?;
        armed += 1;
    }
    Ok(armed)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `n` notes no reading has touched, the last the newest.
    async fn unread(core: &Core, n: i64) -> Vec<String> {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let mut ids = Vec::new();
        for i in 0..n {
            let id = core
                .store
                .insert_artifacts(
                    &src.id,
                    &[crate::store::artifacts::NewArtifact {
                        ordinal: i,
                        text: format!("note {i}"),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap()
                .remove(0)
                .id;
            sqlx::query!(
                "UPDATE artifacts SET created_at = ? WHERE id = ?",
                1_000 + i,
                &id
            )
            .execute(&core.store.pool)
            .await
            .unwrap();
            ids.push(id);
        }
        ids
    }

    async fn armed(core: &Core, ids: &[String]) -> Vec<bool> {
        let mut out = Vec::new();
        for id in ids {
            out.push(core.store.live_job(Stage::Read, id).await.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn notes_from_before_readings_existed_are_read_newest_first_while_the_base_is_quiet() {
        let mut core = crate::core::test_support::test_core().await;
        core.decide_cfg.max_per_tick = 4;
        let ids = unread(&core, 5).await;
        let r = run(&core).await.unwrap();
        assert_eq!(r.scanned, 2);
        assert_eq!(armed(&core, &ids).await, [false, false, false, true, true]);
    }

    #[tokio::test]
    async fn the_scan_stops_when_the_person_is_active() {
        let mut core = crate::core::test_support::test_core().await;
        core.decide_cfg.max_per_tick = 4;
        let ids = unread(&core, 3).await;
        core.store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "a search just now".into(),
                    door: crate::store::feedback::Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        let r = run(&core).await.unwrap();
        assert_eq!(r.scanned, 0);
        assert_eq!(armed(&core, &ids).await, [false, false, false]);
    }

    #[tokio::test]
    async fn the_scan_takes_at_most_half_of_what_the_tick_has_left() {
        let mut core = crate::core::test_support::test_core().await;
        core.decide_cfg.max_per_tick = 10;
        let now = crate::store::now();
        for i in 0..4 {
            let id = format!("spent-{i}");
            sqlx::query!(
                "INSERT INTO decisions (id, question_id, question_version, model, era, subject_kind,
                   subject_a, answer, top, pick, band, state, decided_at)
                 VALUES (?, 'pair_relation', 1, 'm', 'e', 'pair', 'x', '{}', 0.5, 'distinct', 'leave', 'done', ?)",
                id,
                now
            )
            .execute(&core.store.pool)
            .await
            .unwrap();
        }
        assert_eq!(core.judging_budget().await.unwrap(), 6);
        let ids = unread(&core, 6).await;
        // Already queued (as after its embed): skipped, and its slot is not given to an older note.
        crate::jobs::read::arm(&core, &ids[4]).await.unwrap();
        let r = run(&core).await.unwrap();
        assert_eq!(r.scanned, 2);
        assert_eq!(
            armed(&core, &ids).await,
            [false, false, false, true, true, true]
        );
        assert_eq!(
            core.judging_budget().await.unwrap(),
            6,
            "a reading is not a decision, so the pair judge keeps its budget"
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
