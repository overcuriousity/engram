# Contained mode, part 6 — background work and local reminders — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A contained phone does its retrieval work at once and its model work only when it can afford to, never queues work for a model that does not exist, and rings its own reminders.

**Architecture:** The job queue gains one gate, at the claim: while it is shut, the stages that call a generation model are passed over, untouched and unpenalised, and everything else runs as before. A server never shuts it. The contained core shuts it at start and opens it only when the app says a background pass is on and an endpoint exists to do the work. On the app side one WorkManager pass opens the gate under charging, idle and battery-not-low, watches the thermal status, and shuts it again. Reminders are read from the core's own listing, kept in Room, and rung by AlarmManager without the core having to be up.

**Tech Stack:** Rust (sqlx, the existing job queue), JNI (one more function), Kotlin, WorkManager, AlarmManager.

**Spec:** `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md`, section 3 and "What differs" in section 4. Task 1 corrects it.

## Global Constraints

- Rust from the repository root: `cargo test --features contained --lib <filter>`. Baseline 2920 passed. Kotlin from `android/`: baseline 296 tests, 5 skipped, 0 failed.
- The server build's behaviour does not change: its gate is open and nothing reads it as shut.
- A job held by the gate keeps its attempts, its `run_after` and its place. Being held is not a failure.
- No permission that needs a person's grant is added for reminders. Alarms are inexact-while-idle; a reminder may ring minutes late and never needs `SCHEDULE_EXACT_ALARM`.
- UI copy is a term and a short gloss. The one line this part adds to Settings is listed below.
- No device testing. The risk, stated once: WorkManager's idle constraint, doze-time alarms and the thermal API behave on a phone as they are documented to, and nothing here has checked.

## What reading the code found

1. **Capture is already verbatim-first.** `capture_verbatim` cuts a window into passages, embeds them, and only then arms one `SegmentWindow` for a small capture; larger ones wait for use to promote them. So "do not queue synthesis without an LLM" is not a change to capture. It is a gate on the stages that call a model.
2. **Those stages are armed from many places** — capture, promotion, five sweeps. Guarding each arming would be a dozen edits that the next arming site forgets. The claim is one place.
3. **A local ask model cannot synthesize.** `LocalCompleter` implements `Completer`; synthesis is `HttpSynthesizer`, with its own chat, repair and structured-output handling, and no in-process implementation exists. Whether a 2B model can follow engram's synthesis instructions at all is a measurement, and measurements are part 7's. So until then model work in contained mode runs only through an endpoint, and the spec's "if an ask model is installed or an endpoint is set" becomes "if an endpoint is set".
4. **An endpoint set for ask should serve synthesis too.** It is the same OpenAI-compatible server, and a person who sets one expects their captures read. `Setup.ask` is rendered into both tiers.
5. **The due listing already reaches forward.** `GET /api/v1/moments?kind=due&to=…` lists open reminders up to `to`, overdue ones included, with titles. Local reminders need no new route.
6. **When the core stops.** It does not, of its own accord. Models already leave memory when idle (part 2), a job cut off by process death is reclaimed by the repair ticker, and a core that stopped on every trip to the background would cut off the embedding of the capture just shared. `stop` remains for switching mode and for restarting onto new models.
7. **`push`, `pair()` and `unpair()` in contained mode.** Settings no longer offers any of them there. `Engram.pair` while contained is reachable only through a pairing link, and means "use that server": it switches mode first.

## File structure

- Modify `src/store/jobs.rs` — `Stage::calls_a_generator`, `claim_job_holding`.
- Modify `src/tenants.rs`, `src/jobs/mod.rs` — the gate, and `run_any` consulting it.
- Modify `src/contained.rs` — gate shut at start; `Running::allow_generation`; the endpoint in both tiers; `waiting` count.
- Modify `src/web/api.rs` — `status.waiting_generation`.
- Modify `android/native/src/lib.rs`, `Core.kt` — `Core.background(allow)`.
- Create `android/core/.../contained/BackgroundPass.kt` — the worker and its scheduling.
- Create `android/core/.../reminders/LocalReminders.kt` — what to ring and when, pure; and the sync from the core.
- Create `android/app/.../push/AlarmReceiver.kt`, modify `Reminders.kt`, `AndroidManifest.xml` — ringing, and re-arming after a reboot.
- Modify `Db.kt` (two DAO queries, no schema change), `Engram.kt`, `SyncWorker.kt`, `ModeSettings.kt`, `Models.kt` (`Status.waitingGeneration`).

---

### Task 1: The spec, corrected

- [ ] Section 3, tier 2: replace "It runs only if an ask model is installed or an endpoint is set; with an endpoint the constraint is an unmetered network instead, since the phone is not the one computing." with "It runs only where an endpoint is set, and then also wants an unmetered network. The model on the phone answers questions and nothing else until the device pass has measured whether it can read a capture; until then its work waits in the queue, held and not failed." Add to the section: "The core does not stop when the app leaves the foreground. Its models leave memory when idle, and a job cut off with the process is reclaimed at the next start."
- [ ] Commit with this plan: `docs(android): the plan for contained mode, part 6, and the spec corrected by it`.

---

### Task 2: A gate at the claim

**Interfaces — Produces:** `Stage::calls_a_generator(self) -> bool`; `Control::claim_job_holding(&self, held: &[Stage]) -> Result<Option<(String, Job)>>` with `claim_job()` calling it with `&[]`; `Control::waiting_on(&self, subject: &str, stages: &[Stage]) -> Result<i64>`; `Tenants::generation() -> Arc<AtomicBool>` (true by default); `Stage::GENERATORS: [Stage; 7]`.

- [ ] **Step 1: Failing tests** in `src/store/jobs.rs` tests:

```rust
    #[tokio::test]
    async fn a_held_stage_is_passed_over_and_not_penalised() {
        let s = test_store().await;
        s.enqueue(Stage::SegmentWindow, "segment", "c#0").await.unwrap();
        s.enqueue(Stage::Embed, "corpus", "c").await.unwrap();
        let (_, first) = s.control.claim_job_holding(&Stage::GENERATORS).await.unwrap().unwrap();
        assert_eq!(first.stage, Stage::Embed);
        assert!(s.control.claim_job_holding(&Stage::GENERATORS).await.unwrap().is_none());
        assert_eq!(s.control.waiting_on(&s.subject, &Stage::GENERATORS).await.unwrap(), 1);
        let (_, held) = s.control.claim_job().await.unwrap().unwrap();
        assert_eq!((held.stage, held.attempts), (Stage::SegmentWindow, 1), "the wait cost it an attempt");
    }

    #[test]
    fn the_stages_that_call_a_generator_are_the_seven() {
        let named: Vec<_> = Stage::ALL.into_iter().filter(|s| s.calls_a_generator()).collect();
        assert_eq!(named, Stage::GENERATORS);
    }
```

(`test_store` and `s.subject` are whatever the neighbouring tests in that module use; `attempts` is 1 on a first claim there, as the existing claim tests show — match them.)

- [ ] **Step 2: Implement.** `calls_a_generator` is an exhaustive match, `true` for `SegmentWindow`, `Title`, `Dedupe`, `LinkJudge`, `Generate`, `Reap`, `Condense`, with the doc comment: "Does running this make a generation call? Asked at the claim, where a base with nothing to generate with passes these over. `Describe` is not here: it has a role of its own, and its absence is already a wait." `claim_job_holding` is `claim_job`'s statement with `AND stage NOT IN (…)` built from `held` (placeholders, not interpolation), omitted when `held` is empty. `Tenants` holds `generation: Arc<AtomicBool>`; `run_any` claims with `&Stage::GENERATORS` when it reads false.
- [ ] **Step 3:** `cargo test --features contained --lib store::jobs:: jobs::` passes. Commit `feat(jobs): a base with nothing to generate with passes that work over instead of failing it`.

---

### Task 3: The contained core shuts the gate, and the app may open it

**Interfaces — Produces:** `Running::allow_generation(&self, allow: bool) -> bool` (what the gate now is: `allow && an endpoint was given`); `config_for` renders the endpoint into `device-synthesize` as well; `status` JSON gains `waiting_generation`.

- [ ] **Step 1: Failing tests** in `contained::tests`: `an_endpoint_serves_synthesis_too` (the parsed config's synthesize role carries the endpoint's URL, model and key; its `context_tokens` is still 32768); `with_no_endpoint_the_gate_never_opens` (`start`, `allow_generation(true)` is false; ingest a short text, wait for it to reach `ready`, and `waiting_generation` in `/api/v1/status` is 1 with no job failed — read `last_error IS NOT NULL` count through the control store and expect 0); `with_an_endpoint_the_app_decides` (`start_with` an endpoint at a closed port: gate false after start, true after `allow_generation(true)`, false after `allow_generation(false)`).
- [ ] **Step 2: Implement**, then `cargo test --features contained --lib contained::`. Commit `feat(contained): model work waits for an endpoint and for the app's word`.

---

### Task 4: `Core.background`

- [ ] `lib.rs`: a fourth export, `Java_…_Core_background(env, class, allow: jboolean) -> JString`, answering `{"open": bool}` or `{"error": …}` when nothing runs. `Core.kt`: `fun background(allow: Boolean): Boolean`, false where unavailable. Build with `cargo +stable ndk`; four `Java_` symbols. Commit `feat(android): the app can tell the core when model work is affordable`.

---

### Task 5: The pass

**Interfaces — Produces:** `BackgroundPass` (a `CoroutineWorker`); `Passes.schedule(context, engram)` / `Passes.cancel(context)`; `internal fun Passes.constraints(): Constraints`; `internal fun shouldEnd(thermal: Int, waiting: Int): Boolean`; `Engram.passWanted: Boolean` (contained, ask set to `endpoint`, an endpoint stored); `Status.waitingGeneration: Int` (`@SerialName("waiting_generation")`, default 0).

- [ ] **Step 1: Failing tests** `BackgroundPassTest`: constraints are charging, device idle, battery not low, unmetered; `shouldEnd` is true at `THERMAL_STATUS_MODERATE` and above whatever waits, true at zero waiting, false otherwise; `EngramModesTest.aPassIsWantedOnlyWhereAnEndpointWouldDoTheWork`.
- [ ] **Step 2: Implement.** The worker: not wanted → success. `ready()`, `Core.background(true)`, then every 30 s read the thermal status and `GET /api/v1/status`; end on `shouldEnd`; `finally { Core.background(false) }`, which is also what cancellation by a lapsed constraint reaches. Periodic, every 6 h, `KEEP`. `Passes.schedule` is called from `Engram`'s construction in contained mode and after `AskSection` saves; `cancel` from `shutdown`.
- [ ] **Step 3:** tests pass. Commit `feat(android): one background pass, while charging and idle and cool`.

---

### Task 6: Reminders the phone rings itself

**Interfaces — Produces:** in `core`: `data class Ring(val id: String, val title: String, val at: Long)`; `object LocalReminders { fun plan(rows: List<DueRow>, now: Long): List<Ring>; suspend fun sync(engram: Engram): List<Ring> }`; `MomentsDao.all()`, `MomentsDao.deleteExcept(ids)`, `MomentsDao.get(id)`. In `app`: `AlarmReceiver`, `Alarms.set(context, rings)`, `BootReceiver`.

- [ ] **Step 1: Failing tests** `LocalRemindersTest`: an undated row rings nothing; a future row rings at its `at`; a snoozed row rings at `snoozedUntil`; an overdue row rings once, now, and not again once `MomentRow.fetchedAt` records it was rung (kept as `notified` in the plan's input); rows are ordered by time; a title falls back to the opening where the row is not named.
- [ ] **Step 2: Implement.** `sync`: contained only; `reader`-free, straight through `transport().get("/api/v1/moments", kind=due, to=now+30 days)`; upsert into `moments`, delete what is no longer listed, return the plan. Called after a drain in `SyncWorker` and from `MainActivity.onResume`. `Alarms.set`: one `setAndAllowWhileIdle(RTC_WAKEUP, …)` per ring, `PendingIntent` keyed by the moment's id so a re-set replaces. `AlarmReceiver`: reads the row from Room and calls `Reminders.show(context, Payload.Due(at, listOf(Moment(id, title, at)), 0))` — the same notification, the same Done and Snooze, which go through the outbox to the core. `BootReceiver` (`RECEIVE_BOOT_COMPLETED`): re-sets from Room, no core needed.
- [ ] **Step 3:** tests pass; `:app:lintDebug` clean. Commit `feat(android): a contained phone rings its own reminders`.

---

### Task 7: The line in Settings, and pairing from contained mode

- [ ] `ModeSection`, in contained mode, under the switch: one `SettingsLine`, muted — `Background · nothing waiting`, `Background · <n> waiting · no endpoint`, or `Background · <n> waiting · charging and idle`, from `Status.waitingGeneration` and `engram.passWanted`. Tested as a pure `backgroundWords(waiting, passWanted)` in `WordsTest`'s style.
- [ ] `Engram.pair` in contained mode: `switch(app, Mode.server)` first, then pair on the new instance — done in `PairScreen`'s caller, since an instance cannot outlive its own switch. Test in `EngramModesTest` at the level it can reach: `pair` on a contained instance throws `IllegalStateException("contained")`, and `Nav` routes a `pairText` arriving in contained mode through `Engram.switch` before `PairScreen`.
- [ ] Commit `feat(android): what waits in the background is said once, in Settings`.

---

### Task 8: Checked

- [ ] Both full suites in the background, real totals and exit codes.
- [ ] Desktop runner: three captures, wait until no job is pending or running that is not held, and confirm through the API that every capture is `ready`, that it stays `ready` five minutes later, and that the log has no `job failed` line. This is the check parts 4 and 5a could not pass.
- [ ] `.so` size. Memory note.
