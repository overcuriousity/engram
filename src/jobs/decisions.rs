//! `Stage::Decisions`: lapse unanswered rows, close outcome windows, apply waiting acts, seat waiting asks,
//! while the base is quiet, queue readings for the notes that have none, and queue relevance labels
//! for recorded searches, and while it stays quiet, queue deep looks at what searches left out.

use crate::core::Core;
use crate::core::judging::{reask_changed, settle_counted};
use crate::error::Result;
use crate::store::decision_log::{Logged, State};
use crate::store::deep_looks::{DeepKind, DeepSource};
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
    pub deepened: u64,
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
            close_unanswerable(core, &d).await;
            continue;
        };
        if !wrote(
            &d,
            "close an orphaned decision",
            core.store.close_unsettled(&d.id).await,
        )
        .unwrap_or(false)
        {
            continue;
        }
        if let Err(e) = applier.reopen(core, &d).await {
            tracing::warn!(decision = %d.id, error = %e, "could not ask an orphaned decision again");
            // One statement: it is never waiting without `unsettled`, so nobody claims it meanwhile.
            wrote(
                &d,
                "put an orphaned decision back",
                core.store.restore_unsettled(&d.id).await,
            );
            continue;
        }
        r.reasked += 1;
    }
    // A question switched off is left out by the store: its acts cannot be carried out, and at
    // the top of the list they would keep the sweep from ever reaching the ones that can.
    let held = core.appliers.disarmed(core);
    for d in core.store.waiting_acts_but(&held, ACTS_PER_RUN).await? {
        let Some(applier) = core.appliers.get(&d.question_id) else {
            close_unanswerable(core, &d).await;
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
                if !wrote(
                    &d,
                    "close a changed decision",
                    core.store.claim_act(&d.id).await,
                )
                .unwrap_or(false)
                {
                    continue;
                }
                match applier.reopen(core, &d).await {
                    Ok(()) => {
                        wrote(
                            &d,
                            "say why a decision was closed",
                            core.store.set_decision_reason(&d.id, "changed").await,
                        );
                        r.reasked += 1;
                    }
                    // Put back as it was, so the next sweep finds it changed and tries again.
                    Err(e) => {
                        tracing::warn!(decision = %d.id, error = %e, "could not ask a changed subject again");
                        wrote(
                            &d,
                            "put a changed decision back",
                            core.store
                                .set_decision_state(&d.id, State::Done, State::Waiting)
                                .await,
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(decision = %d.id, error = %e, "could not read a decision's subjects")
            }
        }
    }
    let max = i64::try_from(core.decide_cfg.ask_open_max).unwrap_or(i64::MAX);
    // Before the seats are counted: a row closed here gives its seat to one that waits.
    for d in core.store.open_asks(max).await? {
        if changed(core, &d).await == Some(true) {
            r.reasked += 1;
        }
    }
    let free = max - core.store.open_ask_count().await?;
    if free > 0 {
        for d in core.store.waiting_asks(free).await? {
            match changed(core, &d).await {
                Some(true) => r.reasked += 1,
                // Not shown as it is; the next sweep looks again.
                Some(false) => {}
                None => {
                    if wrote(&d, "seat an ask", core.store.seat_ask(&d.id, max).await)
                        .unwrap_or(false)
                    {
                        r.seated += 1;
                    }
                }
            }
        }
    }
    // One reading for both: what the scan arms holds its share at once, and the labels' quarter
    // is of the tick, not of what the scan left.
    let left = core.judging_budget().await?;
    r.scanned = scan(core, left / 2).await?;
    r.labelled = label(core, left / 4).await?;
    r.deepened = deepen(core, left / 4).await?;
    core.refresh_model_trails_count().await;
    // Best-effort: a trail left under the old model is only unused until the next quiet tick.
    if quiet(core).await?
        && let Err(e) = core.reembed_stale_trails().await
    {
        tracing::warn!(error = %e, "could not embed old trails again");
    }
    Ok(r)
}

/// Nobody searched, asked or captured for `evolve.idle_secs`.
async fn quiet(core: &Core) -> Result<bool> {
    let idle = crate::store::now() - core.evolve.idle_secs.max(0);
    Ok(!(core.store.activity_since(idle).await? || core.store.captured_since(idle).await?))
}

/// A store write about one decision. A failure is logged and that decision waits for the next
/// sweep; it does not end this one.
fn wrote<T>(d: &Logged, what: &'static str, got: Result<T>) -> Option<T> {
    got.map_err(
        |e| tracing::warn!(decision = %d.id, error = %e, step = what, "a sweep step failed; this decision waits for the next sweep"),
    )
    .ok()
}

/// A waiting act whose question nothing here answers can be neither carried out nor asked again:
/// closed, so it stops taking a place among the acts each sweep looks at.
async fn close_unanswerable(core: &Core, d: &Logged) {
    wrote(
        d,
        "close an act no applier answers",
        core.store.close_waiting_act(&d.id, "no_applier").await,
    );
}

/// Whether an ask is about something that is no longer so. `Some(true)`: closed and its subject
/// put back to be asked again. `Some(false)`: changed or unreadable, and left to the next sweep.
/// `None`: it stands, as does an ask whose question nothing here answers.
async fn changed(core: &Core, d: &Logged) -> Option<bool> {
    let applier = core.appliers.get(&d.question_id)?;
    match applier.unchanged(core, d).await {
        Ok(true) => None,
        Ok(false) => match reask_changed(core, applier.as_ref(), d).await {
            Ok(closed) => Some(closed),
            Err(e) => {
                tracing::warn!(decision = %d.id, error = %e, "could not ask a changed subject again");
                Some(false)
            }
        },
        Err(e) => {
            tracing::warn!(decision = %d.id, error = %e, "could not read a decision's subjects");
            Some(false)
        }
    }
}

/// Searches looked at per sweep for labelling. All of them can run: the store leaves out a search
/// waiting out a failed attempt or given up, so stuck ones never hide the ones behind them.
const LABEL_LOOKAHEAD: i64 = 64;

/// Unlabelled searches that are due, oldest first, within `share`, each costing its live
/// candidates. The first may overrun it when no label unit is in flight; a queued one below the
/// retry ceiling holds its share, and the oldest that does not fit waits rather than being passed.
async fn label(core: &Core, share: usize) -> Result<u64> {
    let mut left = share;
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
        // A unit at the retry ceiling stays queued for the queue to retry, and holds nothing.
        let attempts = core.store.live_job_attempts(Stage::Label, &id).await?;
        if attempts.is_some_and(|a| a >= crate::store::jobs::MAX_ATTEMPTS) {
            continue;
        }
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
        if attempts.is_some() {
            held = true;
            continue;
        }
        crate::jobs::label::arm(core, &id).await?;
        armed += 1;
    }
    Ok(armed)
}

/// Sources looked at per sweep: enough that skipped ones never hide the rest.
pub const DEEPEN_LOOKAHEAD: i64 = 16;
/// What a second reading is counted at: it asks about a decision's few notes, not a whole pool.
const SECOND_READING_COST: usize = 4;

/// Deep looks, while the base is quiet and within `share`: open gaps, then weak decisions a person
/// called right, decisions nobody judged, then searches the labeller left out. One already looked
/// at, waiting out a failure or queued is passed. The first may overrun the share when no look is
/// in flight, as `label` does for its first search.
async fn deepen(core: &Core, share: usize) -> Result<u64> {
    if !core.learn.enabled || share == 0 || !quiet(core).await? {
        return Ok(0);
    }
    let version = crate::core::judging::catalogue::answers_query("").version;
    let (model, embed) = (core.decider.model(), core.embedder.model());
    let store = &core.store;
    let mut sources: Vec<DeepSource> = store
        .open_gaps(embed, core.weak_below())
        .await?
        .gaps
        .into_iter()
        .map(|g| DeepSource {
            kind: DeepKind::Gap(g.gap.kind),
            id: g.gap.id,
        })
        .collect();
    let of = |kind, ids: Vec<String>| ids.into_iter().map(move |id| DeepSource { kind, id });
    sources.extend(of(
        DeepKind::WeakRight,
        store.weak_right_decisions(embed, DEEPEN_LOOKAHEAD).await?,
    ));
    sources.extend(of(
        DeepKind::SecondReading,
        store
            .decisions_unread(model, embed, DEEPEN_LOOKAHEAD)
            .await?,
    ));
    let relevant_at = crate::jobs::label::relevant_at(core).await?;
    sources.extend(of(
        DeepKind::Labelled,
        store
            .searches_left_out(relevant_at, version, model, embed, DEEPEN_LOOKAHEAD)
            .await?,
    ));
    let mut left = share;
    let mut armed = 0;
    // Looks already queued hold their cost, so two ticks cannot both spend the share.
    let in_flight = store.queued_units(Stage::Deepen).await? > 0;
    for s in sources {
        if store.deep_looked(&s, version, model).await?
            || !store.deep_look_due(&s, version, model).await?
            || store.live_job(Stage::Deepen, &s.target()).await?
        {
            continue;
        }
        let cost = if s.kind == DeepKind::SecondReading {
            SECOND_READING_COST
        } else {
            core.decide_cfg.deep_max
        };
        if cost > left && (armed > 0 || in_flight) {
            break;
        }
        left = left.saturating_sub(cost);
        crate::jobs::deepen::arm(core, &s).await?;
        armed += 1;
    }
    Ok(armed)
}

/// Notes no reading has touched, newest first, while nobody searches, asks or captures. Within
/// `share`, so the pair judge is never starved; a note already queued keeps its slot, and one that
/// could not be read holds none: the store leaves it out until it is due again.
async fn scan(core: &Core, share: usize) -> Result<u64> {
    if !quiet(core).await? {
        return Ok(0);
    }
    let half = i64::try_from(share).unwrap_or(i64::MAX);
    if half == 0 {
        return Ok(0);
    }
    let asked = crate::jobs::read::reading_questions();
    let versions: Vec<(&str, u32)> = asked.iter().map(|a| (a.id, a.version)).collect();
    let room = crate::jobs::read::room(core, &asked);
    let mut armed = 0;
    for id in core
        .store
        .artifacts_lacking_readings(&versions, room, half)
        .await?
    {
        if core.store.live_job(Stage::Read, &id).await? {
            continue;
        }
        crate::jobs::read::queue(core, &id).await?;
        armed += 1;
    }
    Ok(armed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A search a person called a gap, long ago and under the live embed model.
    async fn old_gap(core: &Core) -> DeepSource {
        let id = core
            .store
            .record_search(
                crate::store::feedback::NewEvent {
                    fold_onto: None,
                    query: "where did it go".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0, 0.0],
                    embed_model: core.embedder.model().into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        core.store
            .judge(
                &id,
                crate::store::feedback::Verdict::Gap,
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET created_at = 1000 WHERE id = ?",
            &id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        DeepSource {
            kind: DeepKind::Gap(crate::store::gaps::GapKind::Search),
            id,
        }
    }

    #[tokio::test]
    async fn an_open_gap_is_armed_for_a_deep_look_when_quiet() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let s = old_gap(&core).await;
        assert_eq!(deepen(&core, 60).await.unwrap(), 1);
        assert!(
            core.store
                .live_job(Stage::Deepen, &s.target())
                .await
                .unwrap()
        );
        // Queued, it is passed, and it holds its share of the tick.
        assert_eq!(deepen(&core, 60).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn nothing_is_armed_while_someone_searched_inside_the_idle_window() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let s = old_gap(&core).await;
        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            crate::store::now(),
            &s.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(deepen(&core, 60).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_source_already_looked_at_in_this_era_is_passed() {
        let mut core = crate::core::test_support::test_core().await;
        core.learn.enabled = true;
        let s = old_gap(&core).await;
        let version = crate::core::judging::catalogue::answers_query("").version;
        core.store
            .put_deep_look(&s, &[], version, core.decider.model())
            .await
            .unwrap();
        assert_eq!(deepen(&core, 60).await.unwrap(), 0);
    }

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

    /// `n` embedded notes no reading has touched, the last the newest, captured long ago.
    async fn unread(core: &Core, n: i64) -> Vec<String> {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        sqlx::query!("UPDATE corpora SET created_at = 1000 WHERE id = ?", &src.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
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
                "UPDATE artifacts SET created_at = ?, embed_state = 'embedded' WHERE id = ?",
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
    async fn the_scan_waits_out_a_capture_as_it_does_a_search() {
        let mut core = crate::core::test_support::test_core().await;
        core.decide_cfg.max_per_tick = 4;
        let ids = unread(&core, 3).await;
        core.store
            .insert_corpus("captured just now", "ui", None)
            .await
            .unwrap();
        let r = run(&core).await.unwrap();
        assert_eq!(r.scanned, 0);
        assert_eq!(armed(&core, &ids).await, [false, false, false]);
    }

    #[tokio::test]
    async fn the_scan_leaves_a_note_still_to_be_embedded_to_the_read_its_embed_arms() {
        let mut core = crate::core::test_support::test_core().await;
        core.decide_cfg.max_per_tick = 8;
        let ids = unread(&core, 3).await;
        sqlx::query!(
            "UPDATE artifacts SET embed_state = 'pending' WHERE id = ?",
            &ids[2]
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let r = run(&core).await.unwrap();
        assert_eq!(r.scanned, 2);
        assert_eq!(armed(&core, &ids).await, [true, true, false]);
    }

    #[tokio::test]
    async fn thirty_notes_that_cannot_be_read_do_not_keep_the_next_from_being_read() {
        let mut core = crate::core::test_support::test_core().await;
        core.decide_cfg.max_per_tick = 60;
        core.decider = std::sync::Arc::new(crate::infer::fake::ScriptedDecider::rejecting(
            "HTTP 400: no",
        ));
        let ids = unread(&core, 31).await;
        let r = run(&core).await.unwrap();
        assert_eq!(r.scanned, 30);
        assert!(!armed(&core, &ids).await[0], "the oldest waits its turn");
        crate::jobs::test_support::drain(&core).await;
        assert!(
            armed(&core, &ids).await.iter().all(|a| !a),
            "a refused read stayed queued"
        );

        let r = run(&core).await.unwrap();
        assert_eq!(r.scanned, 1, "the unreadable ones were armed again");
        let after = armed(&core, &ids).await;
        assert!(
            after[0],
            "thirty unreadable notes kept the next from its turn"
        );
        assert!(after[1..].iter().all(|a| !a));
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
        assert_eq!(
            core.judging_budget().await.unwrap(),
            5,
            "the queued reading holds its call"
        );
        // Half of five is two: the queued note's slot, and one more.
        let r = run(&core).await.unwrap();
        assert_eq!(r.scanned, 1);
        assert_eq!(
            armed(&core, &ids).await,
            [false, false, false, false, true, true]
        );
        assert_eq!(
            core.judging_budget().await.unwrap(),
            4,
            "the pair judge is left what the readings did not take"
        );
    }

    #[tokio::test]
    async fn two_arming_sweeps_in_one_tick_arm_no_more_than_the_cap_between_them() {
        let mut core = crate::core::test_support::test_core().await;
        core.decide_cfg.max_per_tick = 4;
        let ids = unread(&core, 6).await;
        for pair in ids.chunks(2) {
            core.store
                .record_pair(&pair[0], &pair[1], 0.9)
                .await
                .unwrap();
        }
        let scanned = run(&core).await.unwrap().scanned as usize;
        // The pair judge arms in the same tick, before a single reading has run.
        let judged = crate::jobs::consolidate::arm_dedupe(&core).await.unwrap();
        assert!(scanned > 0 && judged > 0, "{scanned} read, {judged} judged");
        assert!(
            scanned + judged <= 4,
            "{scanned} readings and {judged} pairs armed against a cap of 4"
        );
        assert_eq!(core.judging_budget().await.unwrap(), 4 - scanned - judged);
    }

    #[tokio::test]
    async fn searches_waiting_out_a_failed_label_do_not_use_up_the_lookahead() {
        use crate::store::feedback::{Door, NewCandidate, NewEvent};
        let core = crate::core::test_support::test_core().await;
        let note = crate::core::test_support::seed_artifact(&core, "Loop mounts").await;
        let version = crate::core::judging::catalogue::answers_query("").version;
        // One more search than a sweep looks at, oldest first.
        let mut ids = Vec::new();
        for i in 0..=LABEL_LOOKAHEAD {
            let id = core
                .store
                .record_search(
                    NewEvent {
                        query: format!("search {i}"),
                        door: Door::Ui,
                        scope: None,
                        filters: "{}".into(),
                        query_vec: vec![0.1, 0.2],
                        embed_model: "fake".into(),
                        candidates: vec![NewCandidate {
                            artifact_id: note.clone(),
                            score: 0.5,
                            similarity: Some(0.5),
                            shown: true,
                            ..Default::default()
                        }],
                        answered: false,
                        fold_onto: None,
                        context: None,
                    },
                    0,
                )
                .await
                .unwrap();
            sqlx::query!(
                "UPDATE search_events SET created_at = ? WHERE id = ?",
                1_000 + i,
                &id
            )
            .execute(&core.store.pool)
            .await
            .unwrap();
            ids.push(id);
        }
        // Every one but the newest failed a moment ago and waits.
        let (stuck, healthy) = ids.split_at(LABEL_LOOKAHEAD as usize);
        for id in stuck {
            core.store
                .label_failed(
                    id,
                    version,
                    core.decider.model(),
                    false,
                    crate::store::now() + 3_600,
                )
                .await
                .unwrap();
        }

        let r = run(&core).await.unwrap();
        assert_eq!(r.labelled, 1, "the one search that can run was not reached");
        assert!(
            core.store
                .live_job(Stage::Label, &healthy[0])
                .await
                .unwrap()
        );
        for id in stuck {
            assert!(!core.store.live_job(Stage::Label, id).await.unwrap());
        }
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
