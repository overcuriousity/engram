# Multi-user tenancy

**Status:** approved design, 2026-08-24. Implementation plan not yet written.

Engram becomes a multi-user application. Every user gets their own SQLite
database and their own Qdrant collection. No user data is shared. Every
setting in `config.toml` stays instance-wide.

## The shape of the split

Two planes, divided along what is actually scarce.

**Data plane, per user.** SQLite files and Qdrant collections are cheap and
independent, so they are divided. Isolation is structural rather than a
predicate: there is no query anywhere that could be written without a tenant
filter, because no tenant filter exists.

**Compute plane, instance-wide.** One embed endpoint, one synthesize endpoint,
one reranker, most likely one GPU behind all of them. That is the bottleneck of
the whole system and it is shared whether or not anyone intends it to be. A
worker pool per user would let ten signed-in users fire `10 x server.workers`
concurrent requests at a single endpoint, where throughput does not scale but
collapses, and queueing moves from a place the operator controls into the model
server's socket backlog: invisible, unordered, unfair.

The job queue is where the two planes meet, which is why the tenant belongs
*in* the queue rather than in a registry beside it. It is the only place in the
system where every user's pending work is one ordered list, and therefore the
only place where fairness, priority and backpressure can be reasoned about at
all.

Adding a user costs a file and a collection. It does not cost a thread pool.

## Control plane

A new SQLite database, `store.control_path` (default `engram-control.db`),
holds the tables that are about people rather than knowledge: `sessions`,
`api_tokens`, the new `users` table, and the job queue.

`meta` deliberately stays **per tenant**. It looks instance-wide — it holds the
`embed.recipe` fingerprint — but it also holds the sweep cursors
`EVENTS_AFTER` and `JUDGED_AFTER` (`src/jobs/associate.rs:180,248`) and
`PURSUIT_AFTER` (`src/jobs/pursuit.rs:261`). Shared, one tenant's association
sweep would advance the cursor past another tenant's unprocessed events, which
presents as association silently missing things. The recipe check in
`main.rs:142` therefore moves out of startup and into a tenant's first open,
where it warns about the collection it is actually describing.

```sql
CREATE TABLE IF NOT EXISTS users (
  subject      TEXT PRIMARY KEY,      -- OIDC sub
  email        TEXT,
  slug         TEXT NOT NULL UNIQUE,  -- filesystem- and collection-safe key
  can_judge    INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL
);
```

`slug` is derived once at provisioning time as a hex SHA-256 prefix of the
subject. Not from the email: an OIDC subject may contain arbitrary characters
and an email can change, while a hash is stable, opaque, and safe in both a
filename and a Qdrant collection name. It is stored rather than recomputed so
the mapping survives any later change to how it is derived.

## Data plane

Everything else — the 19 remaining tables — lives in `{store.dir}/{slug}.db`,
created from the existing `schema.sql` with no changes to it. `Store::migrate`
runs per tenant on first open.

Each tenant reads and writes the Qdrant alias `{vector.collection}_{slug}`,
with the existing generation machinery underneath it untouched.
`QdrantVectors` holds nothing but an HTTP client, a base URL, an alias name and
three ranking scalars, so a per-tenant instance is essentially free to
construct.

`Config` is unchanged except for three new keys: `store.control_path`,
`store.dir`, `migrate.adopt_subject`. `store.path` is kept, and read by
adoption alone: it names the single-user database being taken over, and means
nothing once the `users` table is non-empty. Every model endpoint, threshold
and budget is shared.

## Accounts

`auth.mode = "oidc"`. The first request from an unseen subject provisions the
user. The identity provider owns accounts; engram owns nothing but the mapping.
There is no registration UI and no password management. `auth.mode = "local"`
stays single-user and is for development.

Provisioning is a five-step sequence, each step individually idempotent, so a
crash part-way through is recovered by logging in again rather than by an
operator with a shell:

1. `INSERT OR IGNORE` the `users` row, so a double-click cannot make two.
2. Open or create `{store.dir}/{slug}.db` and run `migrate`.
3. Construct `QdrantVectors` on alias `{vector.collection}_{slug}`.
4. `ensure_collection(cfg.infer.embed.dim)`.
5. Build the `Core`.

Provisioning is *not* transactional across the three systems. A Qdrant outage
during a first login must fail loudly at the door rather than leave a user row
with no collection, which would present as a base that returns empty searches.

## The judge and config-write gate

`users.can_judge` gates the whole `judge_router` — all eleven routes at
`src/web/judge.rs:921-931`. That single gate closes both doors, because
`/ui/judge/tune/{run_id}/apply` is the only route in the tree that writes
`config.toml` (`src/web/judge.rs:860`, `config::write_ranking`).

There is no admin role. The flag is granted out of band, per user.

Three attachment points:

1. A `CanJudge` extractor wrapping `Tenant`. It returns 403 when the flag is
   unset. Every judge handler names it instead of `Tenant`, so a route added
   later without it fails to compile against the pattern the others use rather
   than silently opening.
2. `web::state::judge_pending` returns `None` for an ungranted user, which
   removes the nav entry: an ungranted user is not shown a door they cannot
   open. It already returns `None` when `learn.enabled` is false, so this is
   one more clause in a check that exists.
3. Capture is unaffected. `learn.enabled` stays instance-wide and
   `search_events` are still written per tenant. The flag governs only who may
   *rule* on them and thereby move the instance's ranking parameters.

## Request path

**`core` is removed from `AppState`.** This is the load-bearing decision.
`AppState` keeps `auth`, `config`, `config_path` and `ask_handoff`, and gains
`tenants: Arc<Tenants>`. Dropping the field turns all 187 `st.core` sites into
compile errors, so the compiler enumerates the migration instead of a grep
doing it. There is no path by which a handler quietly keeps talking to a global
core.

```rust
pub struct Tenant {
    pub core: Core,
    pub user: User,   // subject, email, slug, can_judge
}
```

`Tenant` implements `FromRequestParts<AppState>` and resolves in order:
`Identity` (unchanged, already on 78 handlers), registry lookup by subject,
provisioning on first sight. It runs after `Identity`, so an unauthenticated
request still fails in the same place with the same 401 and the redirect
middleware in `src/web/mod.rs` keeps working untouched.

The migration is then mostly substitution rather than rewrite: 78 handlers
already carry `_id: Identity`, an ignored binding in almost every case, which
becomes `t: Tenant`; `st.core` becomes `t.core`. The 18 helper functions taking
`&AppState` also take `&Tenant` where they touch data, and keep only
`&AppState` where they touch config.

**Registry.**

```rust
pub struct Tenants { /* config, qdrant base, LRU of open Cores */ }
impl Tenants {
    async fn get_or_provision(&self, id: &Identity) -> Result<Tenant>;
}
```

An open tenant costs a SQLite pool and a `Background` queue. Two bounds:
per-tenant `max_connections` drops from 8 to 4, and the registry evicts a
tenant that is neither serving a request nor holding a due job, draining its
background queue before dropping it. `store.max_open_tenants` caps the map,
evicting least-recently-used. Eviction is transparent; the next request or
claim reopens the same file.

**Known cost.** `Core` holds per-instance caches — `QueryCache`, `CorpusLocks`,
`Background`. Per-tenant is the correct scope for all three, but the
query-embedding cache no longer amortises across users. That is inherent to the
isolation being asked for and is not worked around.

## Job queue

The `jobs` table moves from the per-tenant schema into the control database and
gains a `subject` column:

```sql
CREATE TABLE IF NOT EXISTS jobs (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  subject     TEXT NOT NULL REFERENCES users(subject) ON DELETE CASCADE,
  stage       TEXT NOT NULL,
  target_kind TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  state       TEXT NOT NULL DEFAULT 'pending',
  attempts    INTEGER NOT NULL DEFAULT 0,
  run_after   INTEGER NOT NULL DEFAULT 0,
  last_error  TEXT,
  claimed_at  INTEGER,
  created_at  INTEGER NOT NULL DEFAULT 0,
  seq         INTEGER NOT NULL DEFAULT 0,
  class       INTEGER NOT NULL DEFAULT 0,
  UNIQUE(subject, stage, target_id)
);
```

A user's `Core` places orders on it; the instance's `server.workers` fulfil
them. `claim_job` claims globally, reads `subject` off the row, resolves that
tenant's `Core` through the registry, and calls the unchanged `run_one(&core)`.
`server.workers` keeps meaning exactly what it means today: how many things
this machine does at once. It is the admission control point for the GPU and it
stays one number however many people sign up.

The blast radius is contained: `Store` gains a control pool and its own
subject, and every query in `src/store/jobs.rs` binds that subject. One file,
1464 lines. Nothing else in `src/store` moves.

**Fairness comes free.** The claim order `(state, class, attempts, seq, id,
run_after)` is unchanged. `seq` is already the intra-batch position — the
comment on `idx_jobs_claim3` says it exists so that every document's first
window runs before any document's second — and across users that same ordering
interleaves batches instead of draining one user's ingest before starting
another's. No scheduler, no weights, no per-user quota. If it proves unfair in
practice that is a later change, with evidence behind it.

`sweep_runs` stays per tenant. It is history a user reads on their own Ops page,
and the worker is already holding that tenant's `Core` when it writes the row.

**One guarantee is given up.** Three call sites in `src/store/corpora.rs`
(lines 316, 373, 592) enqueue inside the capture transaction, so today the
corpus row and its job commit together or not at all. Across two database files
that is no longer possible: SQLite does not promise atomic commit across
attached databases in WAL mode, so `ATTACH` is not a way out. A crash in that
window leaves a corpus with no queued job. That is exactly the case
`src/jobs/reconcile.rs` exists for — its module doc names "a process killed
between two writes" — and it arms unresolved corpora idempotently on the repair
tick. The safety net is unchanged; what changes is that it now covers one more
failure mode.

## Empty-run backoff

A dormant tenant must not cost anything much. Today every sweep is a timer:
`periodic_period` gives each unit an interval, each re-arms itself one interval
out on completion, and `arm_missing_periodic` re-arms anything that died. A
base with nothing to do wakes, queries, finds nothing, and sleeps — forever, by
construction.

Note what that does *not* cost: no model calls. A sweep with nothing to do
makes none. `Consolidate` runs vector queries, `Retention` is pure SQL,
`ArmDedupe` only calls the judge when there are undecided pairs. What a dormant
tenant costs is a wake-up, a file open and a handful of queries per interval.

So the fix is proportionate. There is exactly one place a sweep decides when to
run again, `rearm_periodic` at `src/jobs/mod.rs:289`:

```rust
let at = crate::store::now() + period.as_secs() as i64;
```

The period is multiplied by two for each consecutive empty run, capped at
`schedule.backoff_max_hours` (default 24). A run that did work resets to the
configured period. `run_accounted` already knows which it was, since it writes
the counts into `sweep_runs.detail`; the signal only needs returning.

The reset comes free, and that is what makes this safe. `arm_now`
(`src/store/jobs.rs:409`) already pulls a sleeping unit's `run_after` forward to
zero, and every producer already calls it: capture, judge verdicts, a sitting
going idle. The test `arming_a_sleeping_unit_starts_its_wait_over` covers it.
New data therefore cancels the backoff with no producer changes.

Cost: `rearm_periodic`, the return type of `run_accounted`, one counter (a
column on `jobs`, or the existing empty-run count in `sweep_runs`), one config
key. Failure mode: a sweep runs later than its configured interval on a quiet
base, bounded by the cap and cancelled by any real input. Nothing stalls.
Steady state for a dormant tenant: six sweeps waking once a day each.

### Rejected: a firing rule

The more correct description of this system is a dataflow net. Each sweep is a
transition; it fires only when its input place holds a token; a drained queue
with no new input is a quiescent marking.

| Transition | Input place — token = | Empty means |
|---|---|---|
| `Consolidate` | artifact written or changed since last run | nothing new to compare |
| `ArmDedupe` | undecided rows in `artifact_pairs` | nothing to judge |
| `Associate` | artifact written or changed since last run | no new edges possible |
| `Context` | `interaction_events` since last run | no behaviour to cluster |
| `Pursuit` | a sitting that has gone quiet | nobody searched |
| `Retention` | a row that has crossed its horizon | nothing has expired yet |

`Retention` is the one unit that is genuinely time-driven, but its next token
has a computable time — `min(created_at) + retain_days` — so its timer becomes
a deadline, which is what the net says anyway.

This is not being built. In a timer world a missed arming costs one interval;
in a net, a lost token stalls a transition forever, and silently. That price
lands on every producer: every path that writes an artifact must arm, or
consolidation quietly stops for that tenant. It is the better model and it is
not worth its blast radius against a problem the size of the one measured
above. Recorded here as where this could go later.

## Startup, adoption, CLI

Boot opens the control database and migrates it only. No tenant is opened, so
startup time does not scale with user count. The inference probes in
`startup_checks` are unchanged — they are about config, not data. Two things
leave it, because both are about a collection rather than an endpoint:
`ensure_collection` moves into provisioning, since there is no longer one
collection to ensure, and `embed_recipe_check` moves to a tenant's first open,
per the note in Control plane.

`spawn_repair_ticker` becomes instance-wide and iterates tenants.
`heal_store_drift` does the same but lazily, on a tenant's first open rather
than for every registered user at boot: a hundred users would otherwise mean a
hundred full collection scrolls before the port opens.

**Adoption** runs once, at boot, guarded on the `users` table being empty and
`migrate.adopt_subject` being set. It moves the existing `store.path` file to
`{store.dir}/{slug}.db`, writes the `users` row with `can_judge = 1`, and
renames the existing Qdrant alias onto `{vector.collection}_{slug}`. Renaming
the alias rather than the collections behind it means nothing re-embeds and the
generation history is preserved. If the alias rename fails, the file move is
rolled back: a half-adopted install that boots is worse than one that refuses.
Adoption is a no-op on second boot.

**CLI.** `--reindex`, `--export-eval` and `--recompute-coverage` take a
required `--user <subject>` and operate on that tenant. Omitting it is an error
listing the known subjects rather than a default, since defaulting to an
arbitrary tenant is how the wrong collection gets reindexed.

New subcommands: `--list-users`, `--grant-judge <subject>`,
`--revoke-judge <subject>`, `--delete-user <subject>`. The last removes the
row, the file and the alias behind a confirmation prompt; the queue rows go
with it through `ON DELETE CASCADE`. The raw form stays valid and is documented
in the README:

```
sqlite3 engram-control.db "UPDATE users SET can_judge = 1 WHERE subject = '...'"
```

## Testing

**The existing suite barely moves.** Every `web/` test builds its app through
one function, `test_support::router(core, local)` at
`src/web/test_support.rs:10`. It builds a single-tenant registry around the
passed `Core`, registers a fixed subject, and returns the router. Local auth
mode already yields one `Identity`, so existing tests keep passing with a change
in one function — and that is the point, not an accident. If tenancy needs edits
scattered across the web tests, the extractor boundary is in the wrong place,
and that should be found out early.

New tests, in the order to write them:

1. **Provisioning is idempotent.** Two concurrent first requests for the same
   unseen subject produce one `users` row, one file, one alias. `INSERT OR
   IGNORE` plus a per-subject lock; the test races two extractors.
2. **Isolation.** Two tenants, the same corpus text captured into both. Each
   sees only their own artifact in search, in the corpus list, in the lineage
   view and through the MCP tool. A `GET` of tenant B's artifact id as tenant A
   is a 404 and not a 403, since a 403 confirms the id exists.
3. **The judge gate.** An ungranted user gets 403 on all eleven judge routes and
   no nav entry; a granted user gets through. `--grant-judge` flips it live,
   without a restart.
4. **Queue tenancy.** Jobs enqueued by two tenants interleave under the existing
   claim order, each runs against its own `Core`, and `--delete-user` cascades
   its rows away.
5. **Backoff.** A sweep that finds nothing doubles its `run_after`, caps at
   `schedule.backoff_max_hours`, and `arm_now` resets it. Runs on tokio's paused
   clock, as the pacing gate's tests already do.
6. **Adoption.** An existing `engram.db` with `migrate.adopt_subject` set
   becomes tenant one with `can_judge = 1`, the alias points at the same
   generation collections, and nothing re-embeds. Second boot is a no-op.

`tests/integration_qdrant.rs` gains a two-tenant case, since alias-per-tenant
cannot be tested against `MemoryVectors`.

## Risks

- **The 187 substitution sites.** Removing `core` from `AppState` makes the
  compiler enumerate them, but a mechanical edit at that volume is where a
  handler quietly gets the wrong tenant if a helper closes over a `Core` from
  elsewhere. Mitigation: no `Core` is cloned out of the registry except through
  a `Tenant`, and isolation test 2 is the backstop.
- **Provisioning spans three systems** and cannot be transactional. See
  Accounts.
- **`ask_handoff` is already subject-keyed** (`src/web/state.rs:87`) and needs
  no change. Assert it rather than assume it.
- **Backup and restore gets harder.** One file becomes N files plus a control
  database, and a restore that mixes generations across the two shows up as
  drift. `heal_store_drift` covers it per tenant; the README must say so.

## Non-goals

No admin role. No cross-user views. No per-user config. No quotas or per-user
rate limits. No sharing or collaboration between users. No migration path back
to single-user. No change to the firing model of the scheduler beyond empty-run
backoff.
