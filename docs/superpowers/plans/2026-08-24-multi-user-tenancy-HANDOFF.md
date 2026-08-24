# Multi-user tenancy — handoff, 2026-08-24

**Branch:** `multi-user-tenancy`, five commits off `master` (`f0e3bdf`).
**State:** Tasks 1–5 of 10 done. `cargo test` green: **1793 passed, 0 failed.**
**Spec:** `docs/superpowers/specs/2026-08-24-multi-user-tenancy-design.md`
**Plan:** `docs/superpowers/plans/2026-08-24-multi-user-tenancy.md` — Tasks 6–10 are still to do and are written out step by step.

Read the spec first. It carries the *why*, including what was considered and
rejected; the plan carries the steps.

## What is done

| # | Task | Commit |
|---|------|--------|
| 1 | Control database + `users` table + `slug_for` | `ecc7688` |
| 2 | Sessions and API tokens move to control | `6650b8a` |
| 3 | One job queue in control, keyed by `subject` | `c28e09e` |
| 4 | Tenant registry | `a3fa45c` |
| 5 | `Tenant` extractor; `core` removed from `AppState` | `bbbbdad` |

The architecture is in place and the app compiles and passes its whole suite as
a single-tenant instance running through the multi-tenant machinery. What is
missing is the judge gate, the worker dispatch, the real boot path, the
backoff, and the end-to-end isolation test.

## What is left

**Task 6 — the judge gate.** The smallest remaining task and a good warm-up.
`CanJudge` already exists (`src/web/tenant.rs`) and already returns 403 when
`users.can_judge` is 0. Nothing uses it yet: swap `tenant: Tenant` for
`CanJudge(tenant): CanJudge` in the eleven handlers of `judge_router`
(`src/web/judge.rs:921-931`). `judge_pending` already returns `None` for an
ungranted user, so the nav entry disappears on its own.
`test_support::router_ungranted` is written and waiting for a caller.

**Task 7 — workers claim globally.** `Control::claim_job` already returns
`(subject, Job)`. `run_one` in `src/jobs/mod.rs` still takes a `&Core` and
throws the subject away (`let Some((_subject, job)) = ...`). It needs to take
`&Tenants`, resolve the subject through `Tenants::get`, and drop a job whose
user was deleted rather than retrying it. `spawn_repair_ticker` in
`src/core/background.rs` needs to iterate `control.users()`.

**Task 8 — boot, adoption, CLI.** `src/main.rs` currently runs the whole server
as one hardcoded subject, `BOOT_SUBJECT = "single-user"`, provisioned at
startup, with `Tenants::single` around it. **This is interim scaffolding and is
the most important thing to replace.** It is marked as such in the source.
Task 8 in the plan replaces it with the control-only boot, `adopt()`, and the
`--user` / `--grant-judge` / `--revoke-judge` / `--list-users` /
`--delete-user` flags.

**Task 9 — empty-run backoff.** Untouched, except that the `empty_runs` column
already exists in `control_schema.sql`, so the schema step is done.

**Task 10 — isolation tests and docs.** Untouched.

## Deviations from the plan, and why

1. **`Store` kept its job methods** rather than delegating into `impl Control`
   for all 21. They run against `self.control.pool` and bind `self.subject`.
   Same guarantee — every queue query binds a subject — at a fraction of the
   churn. Only the genuinely instance-wide ones (`claim_job`, `complete_job`,
   `fail_job`, `reclaim_stuck`, `age_background`) live on `Control`.
   `Store::claim_job` also exists, claiming only that tenant's work: 93 test
   call sites use it, and "take this base's next step" is a real operation.

2. **Four production queries are no longer joins.** They filtered `artifacts`
   by `NOT EXISTS (SELECT 1 FROM jobs …)`, which cannot cross two databases.
   They now ask the tenant for candidates and `Control::targets_with_jobs`
   which are spoken for:
   - `Store::pending_artifacts_are_isolated` (`src/store/artifacts.rs`)
   - `Store::list_unrelated_artifact_ids` (same file)
   - `Store::stranded_merges` (`src/store/lineage.rs`)
   - the `judge_queue` count (`src/store/links.rs`), now `Control::live_count`

   **The two with a `LIMIT` over-fetch 8× before filtering**, because the limit
   has to apply after the queue test. That factor is a guess. It is the one
   place in this work where behaviour, not just structure, changed — worth a
   look under real data, and worth saying out loud in the PR.

3. **`meta` stays per tenant**, against what the spec's first draft said. It
   holds the sweep cursors `EVENTS_AFTER`, `JUDGED_AFTER` and `PURSUIT_AFTER`;
   sharing it would let one tenant's association sweep step over another's
   unprocessed events. The spec was corrected before the plan was written.

4. **`Store::migrate`'s `backfill_job_class` moved to `Control::migrate`**,
   since the rows it corrects are no longer in the tenant database.

5. **`TEST_SUBJECT` is `"user-1"`**, not something tidier, because that is the
   subject the web fixtures have always signed in as. The identity at the door
   and the owner of the rows behind it have to be one person.

6. **`Tenants::single` answers for any subject** (`solo: true`). That is what
   `auth.mode = "local"` is — one account, one base — and the local username is
   not the same string as the subject the data was written under. Isolation is
   never tested through it: `tenants::test_support::test_tenants` builds a real
   registry, and the cross-tenant tests use that.

7. **`Config::test_default()` is not behind `cfg(test)`.** The binary's own
   tests compile this crate as a dependency, where that flag is not set.

8. **MCP builds one service per tenant**, cached in `mcp_router`, rather than
   one per process. `StreamableHttpService` is constructed with the tools, and
   the tools hold a `Core`, so a single service would have been one user's data
   behind everyone's bearer token. Each tenant also gets its own
   `LocalSessionManager`, which is what an MCP session should be.

## Known rough edges

- **84 build warnings**, almost all `unused variable: st`: 70 handlers still
  take `State(st): State<AppState>` they no longer read. Harmless, noisy,
  and a five-minute mechanical pass — remove the extractor where the body
  never touches `st`. Also 8 now-unused `use crate::auth::Identity`.
- **`BOOT_SUBJECT` in `src/main.rs`** — see Task 8 above. The server is not
  actually multi-user until that is replaced.
- **No clippy on this machine.** `cargo` is `/usr/bin/cargo` with no rustup, so
  `cargo clippy` does not exist and every task was gated on `cargo test` alone.
  `sudo dnf install clippy`, then run it over the whole branch before the PR.
- **`heal_store_drift`** still runs from `startup_checks` against the one boot
  core. Task 7 moves it into `Tenants::open` so it runs per tenant, lazily.

## How to verify where you are

```
git log --oneline master..HEAD     # five commits
cargo test                         # 1793 passed, 0 failed
cargo build 2>&1 | grep -c warning # 84, all cosmetic
```

The tenancy tests worth reading first, because they say what the machine is
supposed to do:

- `src/tenants.rs` — `racing_first_requests_provision_once`,
  `opening_past_the_cap_evicts_the_least_recently_used`,
  `a_tenants_data_goes_in_its_own_file`
- `src/store/jobs.rs` — `two_tenants_do_not_see_each_others_jobs`,
  `the_same_target_id_in_two_tenants_is_two_jobs`,
  `claiming_says_whose_job_it_is`,
  `deleting_a_user_takes_their_queue_with_them`
- `src/store/mod.rs` — `a_fresh_file_database_gets_the_whole_schema` now
  asserts that `users`, `sessions`, `api_tokens` and `jobs` are **absent** from
  a tenant database.

## The one thing to keep hold of

The split follows the resource, not the user. Data is per tenant because files
and collections are cheap and independent. Compute is instance-wide because
there is one set of inference endpoints and probably one GPU behind them, and
`server.workers` is the admission point in front of it — it must stay one
number however many people sign up. The queue is where those two planes meet,
which is why the subject is a column in it rather than a registry beside it.
