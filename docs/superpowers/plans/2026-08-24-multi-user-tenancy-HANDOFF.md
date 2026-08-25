# Multi-user tenancy — done, 2026-08-25

**Branch:** `multi-user-tenancy`, ten commits off `master` (`f0e3bdf`).
**State:** all ten tasks done. `cargo test` green: **1819 across six binaries,
0 failed**. `cargo clippy --all-targets -- -D warnings` clean.
**Spec:** `docs/superpowers/specs/2026-08-24-multi-user-tenancy-design.md`
**Plan:** `docs/superpowers/plans/2026-08-24-multi-user-tenancy.md`

| # | Task | Commit |
|---|------|--------|
| 1 | Control database + `users` table + `slug_for` | `ecc7688` |
| 2 | Sessions and API tokens move to control | `6650b8a` |
| 3 | One job queue in control, keyed by `subject` | `c28e09e` |
| 4 | Tenant registry | `a3fa45c` |
| 5 | `Tenant` extractor; `core` removed from `AppState` | `bbbbdad` |
| — | The warning backlog task 5 left | `76239fa` |
| 6 | The judge gate | `ea0d7c6` |
| 7 | Workers claim globally | `f3e74f9` |
| 8 | Boot, adoption, CLI | `ef32558` |
| 9 | Empty-run backoff | `e842eb3` |
| 10 | Isolation tests and docs | `5a06225` |

`BOOT_SUBJECT` is gone. The server is actually multi-user.

## Deviations in tasks 6–10, and why

1. **`run_one` was not replaced, it was joined.** The plan had `run_one` take a
   `&Tenants`. It has ~40 callers, all of them test helpers driving a capture to
   completion, and "take *this base's* next step" is a real operation with a
   query already written for it (`Store::claim_job`). So `run_any(&Tenants)` is
   the worker's door — global claim, resolve the subject, dispatch — and
   `run_one(&Core)` stayed as the per-tenant one. Same guarantee, no churn.

2. **The repair tick has two halves, not one.** The plan said to loop over users
   calling `reclaim_stuck`, `age_background` and `arm_missing_periodic` against
   each. The first two are already instance-wide queries on one control
   database; running them per user asks the same question N times and acts on
   the first answer. `repair_control_once` runs them once per tick, and
   `repair_once` runs the genuinely per-tenant passes for each user. Session
   expiry joined them, having lost its home when `startup_checks` shrank.

3. **`embed_recipe_check` moved to `src/tenants.rs`,** not just its call site.
   `main.rs` had the only copy and the binary is a separate crate, so leaving it
   there would have meant two definitions. It is `pub` and runs from
   `Tenants::on_first_open`, on the tenant's background queue alongside
   `heal_store_drift` — a first request must not wait out a collection scroll.

4. **`adopt` takes the alias rename as a parameter.** The plan called
   `QdrantVectors::connect` inside it, which would have made every adoption test
   need a live Qdrant. The three things that decide whether adoption is *safe*
   are the guard, the row and the file, and none of them needs a vector store to
   be wrong. `main` passes the real rename; the tests pass a closure. The
   rename's own behaviour has a live case in `integration_qdrant.rs`.

5. **Adoption moves the WAL sidecars too,** and rolls them back with the file.
   A `-wal` left behind is a committed write that never reaches the base it was
   written for, which reads afterwards as data that was there yesterday.

6. **`--delete-user` drops the generations, not only the alias.** The spec says
   "the alias"; an alias per tenant means nothing else points at what is behind
   it, so stopping at the alias would leave a deleted user's vectors on disk for
   ever. The confirmation prompt says which it is doing.

7. **The backoff tests do not use tokio's paused clock,** though the plan said
   to. A paused clock makes sqlx's pool time out acquiring its first connection
   — the acquire timeout fires before any real work can happen. The tests never
   sleep, so wall clock costs them a second of imprecision against periods
   measured in minutes, and they assert within a two-second tolerance.

8. **`test_support::router_ungranted` was deleted rather than called.** It could
   not have a caller: the judge gate is only reachable by someone signed in, and
   a bare router carries no session. `app_with_cookie_ungranted` replaces it —
   the same fixture with a real session and the grant withheld, which is the
   only difference the gate is allowed to be answering.

9. **`Tenants::single` is now test-only.** Local mode goes through the real
   registry: its subject is the configured username, so one account provisions
   one tenant like anybody else.

## Worth a look before merging

- ~~**The 8× over-fetch from task 2's deviation**~~ — resolved. The factor was
  not a tuning question but a correctness one: a relate row survives its
  completion, so on any base past `8 × limit` artifacts the oldest window is
  entirely armed and `list_unrelated_artifact_ids` returned empty for ever,
  blind to everything behind it. It now walks the base behind a cursor in
  `meta` and wraps at the end, so the factor is only a batch size.
  `stranded_merges` had the same shape over a set that empties itself, and is
  now paged instead of over-fetched.

- **Two wall-clock-sensitive tests flake under heavy load**, roughly one full
  `cargo test --lib` run in ten while a build or clippy is competing for the
  machine. Neither is new code and neither reproduces in isolation:
  - `jobs::associate::tests::a_strong_cross_corpus_link_is_armed_for_the_judge_exactly_once`
    — link decay is computed against `store::now()`, so a slow run decays the
    weight below the arming threshold.
  - `jobs::tests::a_refused_window_backs_off_further_every_time_it_is_refused`
    — reads `run_after - now()`, which shrinks by however long the read took.

  Seventeen pre-change runs were clean and two of about twenty post-change runs
  were not, so this branch perturbs the timing rather than causing it. Both want
  an injected clock rather than a tolerance; that is its own change.

- **`config.toml` in the repo root is stale** and no longer loads —
  `infer.ask.tier` and `infer.synthesize.tier` are missing. Unrelated to this
  branch (it predates it), but it means `./engram` with no `--config` fails in a
  working tree. `config.example.toml` is current and was verified with
  `--print-config`.

## What to run

```
cargo test                                    # 1819, 0 failed
cargo clippy --all-targets -- -D warnings     # clean
cargo test --test integration_qdrant -- --ignored two_tenants_get_two_collections
cargo test --test integration_qdrant -- --ignored renaming_an_alias
```

The last two need a Qdrant on `:6333` and are the half `MemoryVectors` cannot
cover.

## The one thing to keep hold of

The split follows the resource, not the user. Data is per tenant because files
and collections are cheap and independent. Compute is instance-wide because
there is one set of inference endpoints and probably one GPU behind them, and
`server.workers` is the admission point in front of it — it must stay one
number however many people sign up. The queue is where those two planes meet,
which is why the subject is a column in it rather than a registry beside it.
