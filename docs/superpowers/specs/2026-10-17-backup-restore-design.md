# Backup and restore as one unit

Written 2026-10-17, against `master` at `eb5a795`. Upstream issue engram#70.

## What it is for

An engram instance is three stores that describe one body of knowledge:

- the **control database** (`store.control_path`, default `engram-control.db`):
  users, sessions, API tokens, pair grants, the instance's VAPID key pair, and
  the job queue, whose rows name subjects;
- one **tenant database per user** under `store.dir` (`{slug}.db`): artifacts,
  attachments (as BLOBs — there are no loose files), corpora, lineage, the
  sweep cursors and the `meta` row that pins the embedding recipe;
- one **Qdrant collection per user**, reached through the alias
  `{vector.collection}_{slug}`, which points at a numbered generation
  `{alias}_g{N}` (`src/vector/qdrant.rs:144`).

Nothing in the tree backs any of these up, and `README.md:237-243` names only
the first two. A control database or tenant file restored from one moment and
a collection from another is a base that answers worse for no visible reason:
artifacts SQLite calls embedded have no point, or points describe artifacts no
row names. `Core::heal_store_drift` (`src/core/ingest.rs:1690`) already repairs
the first skew by re-queueing the embed and only *warns* about the second, with
the advice "restore both stores from the same snapshot". This design is that
snapshot: one archive, one command to write it, one command to put it back, and
a verification that says in one word whether the two stores agree afterwards.

Two premises in the issue are corrected here, from the code: `store.dir` holds
SQLite databases, not files beside them, so "file presence" is "one `.db` per
user the control database names"; and `heal_store_drift` reconciles
SQLite ↔ Qdrant, not SQLite ↔ `store.dir`.

## Constraints

- **Rootless, no volume access.** On `hermes` engram runs as a user with no
  sudo; Qdrant is a container whose disk the operator cannot read. Everything
  Qdrant-side goes over its HTTP API, which is how `qdrant.rs` already talks to
  it (`reqwest`, `call` at :1082).
- **No admin role.** The README says so and `--delete-user`, `--reindex` and
  `--list-users` are the pattern: the operator at a shell, the server binary
  reading `config.toml`. Backup and restore follow it. No admin HTTP surface.
- **SQLite is in WAL mode** (`src/store/mod.rs:62`, `control.rs:65`). A `cp`
  of a `.db` while the server runs is a torn copy; only the online backup API
  or `VACUUM INTO` yields a consistent file.
- **The two stores are written separately** and will stay that way; the
  backup cannot be point-in-time across them without stopping the server.
- **Host:** 2 cores, 8 GB, ~50 GB free. Snapshots stream to disk; nothing is
  buffered whole in memory.
- **Archives hold user content and credentials-adjacent rows**; the manifest
  must hold neither.
- Design only. Encryption at rest, scheduling and shipping off host are
  follow-ups.

## Options considered

### A. Offline, point-in-time

Stop the server, copy the files, snapshot Qdrant, start again. Simple and the
only way to get a true point in time. Rejected as the *only* mode: it costs
downtime on every backup, and the drift it avoids is one the code already
knows how to heal. It stays available as a stance the operator can take
(stop, then run the same command) and the spec says what it buys.

### B. Online, ordered, reconcilable — chosen

Run the backup as a second process against the live files and the live Qdrant,
in an order whose only possible skew is the one that heals itself. Document
the guarantee honestly as *reconcilable*, not point-in-time, and make restore
finish the reconciliation instead of leaving it to the next sweep.

### C. Online with server cooperation

Add an endpoint that pauses the background queue and the ingest path, back up
under it, resume. Point-in-time without downtime, but it needs an admin
surface that does not exist, an auth model for it, and a quiesce protocol
across every writer (`Core::capture`, the sweep, the repair ticker, MCP). Too
much new surface for a gain the ordering in B already approximates.

**Lenses that decided it:** existing conventions first (operator-at-a-shell
flags, generation + alias swap from `--reindex`); failure modes (the skew B
leaves is the one `heal_store_drift` repairs); fits this host (HTTP only);
simplest thing that could work (no daemon, no port, no new crate).

## 1. The backup unit

A backup is a **directory** the operator names. It must not exist, or be
empty; engram never writes into a populated directory. A single tarball was
considered and rejected: it needs a `tar` crate or a shell-out, cannot be
streamed into by Qdrant's download, and the operator who wants one runs
`tar` on the directory.

```
<dir>/
  manifest.json
  control.db                       # VACUUM INTO of store.control_path
  tenants/
    <slug>.db                      # VACUUM INTO of store.dir/<slug>.db
  vectors/
    <alias>.snapshot               # Qdrant collection snapshot of the alias' generation
```

Permissions: the directory `0700`, every file `0600`, the same stance
`~/.config/engram/cli.toml` takes.

### Manifest

`manifest.json`, written last, after every file it names exists and has been
hashed. A directory with no manifest is an aborted backup and `--restore`
refuses it.

```json
{
  "format": 1,
  "created_at": 1760700000,
  "engram_version": "0.1.0",
  "mode": "online",
  "embed": { "model": "…", "dim": 1024, "fingerprint": "<sha256 hex>" },
  "vector_backend": "qdrant",
  "control": { "file": "control.db", "sha256": "…", "bytes": 123456, "users": 3 },
  "tenants": [
    {
      "slug": "alice",
      "db": { "file": "tenants/alice.db", "sha256": "…", "bytes": 1,
              "artifacts": 812, "embedded": 810 },
      "vector": { "alias": "engram_alice", "generation": "engram_alice_g3",
                  "file": "vectors/engram_alice.snapshot", "sha256": "…",
                  "bytes": 1, "points": 809, "dim": 1024 }
    }
  ]
}
```

- `embed.fingerprint` is `cfg.infer.embed.fingerprint()` (`src/config.rs:1576`),
  the same string `embed_recipe_check` stores under `meta.embed.recipe`.
- `mode` is `online` or `offline`; the latter is set by `--backup --offline`,
  which only differs in that it insists on acquiring the server lock (§3)
  and so can promise point-in-time.
- `vector` is `null` for a tenant whose vectors live in its own `.db`
  (`SqliteFactory`, contained builds); then the database file is the whole
  unit for that tenant and `vector_backend` is `sqlite`.
- Counts (`artifacts`, `embedded`, `points`) are read from the *copied*
  files and the snapshotted generation, so verification compares against
  what the archive holds, not what the live base held.
- **Not in the manifest:** the Qdrant URL or API key, any subject, email,
  token, session, VAPID material. Slugs only. The manifest is safe to paste
  into an issue; the directory is not.

Checksums are SHA-256 (`sha2` and `hex` are already dependencies).

### What `control.db` carries that an operator should know

Sessions, API token hashes, pair grants and the VAPID private key are in the
control database by design — a restore that lost them would log every device
out and break push. They are in the archive; the archive is therefore as
sensitive as the instance. §6 lists the handling rules.

## 2. Taking the backup: order and guarantee

`engram --backup DIR [--offline]`, in the server binary, reading the server's
`config.toml` like `--reindex` does.

1. **Control database first.** Open `store.control_path` read-only (a plain
   `SqliteConnectOptions`, *no* schema setup — `Control::open` runs
   `control_schema.sql` and must not be the entry point) and run
   `VACUUM INTO '<dir>/control.db'`. `VACUUM INTO` takes a read transaction,
   works on a read-only connection, and produces a compact file with no WAL.
2. **Enumerate users from the copy**, not from the live database. A user
   provisioned after step 1 is absent from both halves of the archive, which
   is consistent; enumerating live would name a tenant the control copy does
   not know.
3. **Per tenant, Qdrant first, then SQLite:**
   a. resolve the alias to its generation (`resolve_alias`), refuse to
      continue if there is none or it dangles (`collection_is_live` is false)
      — report the tenant and go on to the next, the archive is marked
      partial and the command exits non-zero;
   b. `POST /collections/{generation}/snapshots?wait=true`, stream
      `GET /collections/{generation}/snapshots/{name}` into
      `vectors/<alias>.snapshot`, hash while streaming, then
      `DELETE /collections/{generation}/snapshots/{name}` so Qdrant's own
      disk is not left holding a copy;
   c. `VACUUM INTO '<dir>/tenants/<slug>.db'` from a read-only connection;
   d. count `artifacts`, `embedded` (same queries `list_all_artifact_ids` and
      `list_embedded_artifact_ids` use) on the copy, and `points` on the
      generation (`points/count`, exact).
4. Write `manifest.json`. Print a one-line-per-tenant summary.

**Why Qdrant before SQLite.** The two possible skews between the halves are
not symmetric. A row the copy calls embedded with no point in the snapshot is
repaired by `heal_store_drift`: it re-arms the embed job and the point is
re-created at the cost of one embedding call. A point with no row is only
warned about, forever. Taking Qdrant first means anything that landed between
the two captures is a row without a point — the self-healing direction. The
other direction can still arise from a *deletion* in the gap (`delete_artifact`
removes the point, then the row, :1741): the snapshot keeps a point the copy
has no row for. Restore prunes exactly those (§4, step 6), using the same
set difference `heal_store_drift` computes.

**The guarantee.** With `mode: online`: *reconcilable*. Every artifact row in
the archive is restorable; the vectors of rows captured or re-embedded during
the backup window are re-created on first open; points for rows deleted
during the window are removed at restore. Nothing captured before the backup
started is lost. With `mode: offline` (the server lock held): *point-in-time*,
and verification is expected to report zero gaps.

**Why not stop the queue from inside the backup?** It is a separate process.
The only lever it has is the lock in §3, which is all-or-nothing, and that is
`--offline`.

## 3. The server lock

Restore replaces files under a running server by `rename`; the server would
keep the old inode open and the next request would write to a file nobody
sees. Nothing today tells a second process that a server is up.

The server takes `std::fs::File::try_lock()` (stable since 1.89; the crate
pins 1.94) on `<store.control_path>.lock` at startup, before `Control::open`,
and holds the handle for its lifetime. Held → `--restore` and
`--backup --offline` fail fast with
"engram is running (lock at <path>); stop it first". `--backup` without
`--offline` ignores the lock. The lock file is empty and is not part of the
backup. `--delete-user` and `--reindex` would benefit from the same guard and
are noted as a follow-up, not changed here.

## 4. Restore

`engram --restore DIR [--user SLUG] [--reembed] [--force-version]`

**Nothing is mutated until every check in steps 0–1 has passed.**

0. **Archive integrity.** Parse `manifest.json` with `deny_unknown_fields`;
   `format` must be `1`. Every file the manifest names must exist with the
   stated byte length and SHA-256. File names are *not* taken from the
   manifest as paths: they are rebuilt from `slug` and `alias`, and a slug
   that is not `[a-z0-9][a-z0-9_-]*` is refused. (Also exposed alone as
   `engram --backup-check DIR`, exit 0/1.)
1. **Refusal rules**, each with its message:
   - *server running* — lock held (§3).
   - *version skew* — `engram_version` newer than the binary (semver compare)
     → refuse; older → allowed, the schema files are `CREATE … IF NOT EXISTS`
     and the open path migrates forward. `--force-version` overrides. The
     Cargo version is a static `0.1.0` today, so this rule is weak until a
     build-time git sha joins it (open question 4).
   - *recipe mismatch* — `embed.fingerprint` ≠ `cfg.infer.embed.fingerprint()`
     → refuse, unless `--reembed`, which restores the databases and **skips
     the snapshots**: each tenant gets a fresh empty generation at the
     configured `dim` and `heal_store_drift` re-arms every embedded artifact
     on first open. The report says so and estimates the embedding calls
     (`embedded` summed).
   - *dim mismatch* — `vector.dim` ≠ `cfg.infer.embed.dim` is a recipe
     mismatch by construction (dim is in the fingerprint) and follows the
     same rule. `--force-version` never bypasses it: a collection of the
     wrong width cannot be queried, which is what `dimension_mismatch`
     (`qdrant.rs:79`) already refuses.
   - *missing collection* — a tenant with `vector != null` whose snapshot
     file is absent or fails its hash → refuse (covered by step 0); a tenant
     whose live alias is *absent* is fine: restore creates it.
   - *vector backend mismatch* — archive says `qdrant`, build is contained
     (`sqlite`), or the reverse → refuse. No automatic conversion.
   - *`--user SLUG`* — the slug must be in the manifest *and* in the live
     control database; per-tenant restore never touches `control.db`.
2. **Acquire the lock** (§3) and hold it to the end.
3. **Stage SQLite.** Copy `control.db` → `<control_path>.restoring`, each
   tenant → `store.dir/<slug>.db.restoring`; re-hash the staged copy and
   compare. `fsync`.
4. **Qdrant, per tenant.** Compute the next generation name
   `generation_name(alias, newest + 1)` (the `--reindex` scheme, :920ff);
   refuse if it exists ("a previous restore did not finish", same wording
   as reindex). Upload:
   `POST /collections/{target}/snapshots/upload?wait=true&priority=snapshot`,
   multipart field `snapshot`, streamed from the file (`reqwest` already has
   `multipart` and `stream`). Then `await_readable`, `ensure_payload_indexes`
   (it is "called on every path that adopts a collection", :633), and check
   `points/count` equals `manifest.vector.points` — else drop `target` and
   fail the tenant. Finally `point_alias_at(target, replacing = alias
   exists)`. **The previous generation is left in place**, exactly as
   `--reindex` leaves it: it is the rollback, and the report names it with
   the `DELETE /collections/{name}` the operator runs once satisfied.
5. **Swap SQLite.** For each staged file: remove the target's stale `-wal`
   and `-shm` sidecars (a `VACUUM INTO` output has none; a stale `-wal` would
   be replayed into the new file), then `rename(.restoring → .db)`. Tenants
   first, control last; each rename is atomic, and a crash between two leaves
   a state a second `--restore` of the same archive repairs: the rerun
   uploads into the next free generation, re-points the alias, and repeats
   every swap. Safe to repeat, at the cost of one more generation to delete.
6. **Verify and reconcile** (§5). Orphan points are deleted
   (`delete_artifacts`); embedded rows without a point are re-armed with
   `rearm_idle_seq(Stage::Embed, …)` — the two halves of
   `heal_store_drift`, run now rather than on the next open, so the report
   can say what it did.

Per-tenant restore (`--user`) runs steps 3–6 for one slug and skips the
control database.

**Refactor that makes 6 honest:** lift the set computation out of
`Core::heal_store_drift` into `pub(crate) fn store_drift(rows, embedded,
points) -> Drift { orphan_points: Vec<String>, missing_points: Vec<String> }`
in `src/core/ingest.rs`, and have both the sweep and restore call it. One
truth for "what disagrees".

## 5. Restore verification

Runs at the end of `--restore`, and alone as `engram --verify-stores
[--user SLUG]` against the live stores (useful before a backup, too). Output
is one table and one word.

Per tenant:

| check | PASS | REPAIR | FAIL |
|---|---|---|---|
| tenant `.db` present for every user in `control.db` | yes | — | missing file |
| `.db` in `store.dir` not named by `control.db` | none | — | stray file listed |
| alias resolves to a live generation of the configured `dim` | yes | — | dangling / absent / wrong width |
| `meta.embed.recipe` equals the configured fingerprint | yes | — | differs (and `--reembed` was not given) |
| embedded rows without a point | 0 | N, re-armed | — |
| points without a row | 0 after prune | — | prune failed |
| `points` equals embedded rows | yes | off by exactly the re-armed N | otherwise |

Verdict: `PASS` (exit 0), `REPAIR` (exit 2: everything restored, N embeds
queued and the number of calls that costs), `FAIL` (exit 1: at least one FAIL
cell; the report says which, and whether the alias still points at the
previous generation, which it does if step 4 failed before the swap).

The same table, run as `--verify-stores` before taking a backup, tells the
operator whether they are about to archive a base that already disagrees
with itself — a thing the Ops page implies today and nothing states.

## 6. Security

Review points for the CTO, in order of consequence:

1. **Archive sensitivity.** `control.db` carries sessions, token hashes, pair
   grants and the VAPID private key; tenant databases carry every artifact
   and attachment. Directory `0700`, files `0600`, created with
   `OpenOptions::mode` before any byte is written, and the README says the
   archive is the instance.
2. **Manifest has no secrets.** Check the struct: no URL, API key, subject,
   email, token or key material. Slugs, hashes, counts, versions only.
3. **Untrusted archive on restore.** The manifest is attacker-shaped input
   (`deny_unknown_fields`; file paths rebuilt from validated slug/alias, never
   taken verbatim; byte lengths and hashes checked before anything is opened
   with SQLite). The `.snapshot` is opaque to engram and handed to Qdrant,
   which validates it; engram never unpacks it.
4. **Qdrant API key** stays in `config.toml` and is sent only in the header
   `qdrant.rs` already sets; it never appears in the manifest or the report.
5. **No mutation before every check** (§4 step 0–1), and the server lock
   prevents a restore under a live server.
6. **Nothing new listens, nothing needs root**, no `docker` group use: the
   snapshot round-trip is HTTP against the URL already configured.

## 7. Test strategy

**Against the in-memory and SQLite vector stores (unit tests, always run):**

- `VectorStore` gains two defaulted methods, `export_snapshot(into: &Path) ->
  Result<VectorExport>` and `import_snapshot(from: &Path, target: &str, dim)
  -> Result<()>`. `VectorExport::{Embedded, File { sha256, bytes, points, dim
  }}`. `memory.rs` implements export as JSON of its points (test fixture, ~40
  lines); `vector/sqlite.rs` returns `Embedded`. This lets the whole
  orchestration run without Qdrant.
- Backup: directory layout, `0700/0600`, manifest fields, checksums match
  files, counts read from the copy, `VACUUM INTO` from a read-only
  connection while another connection holds a write transaction in WAL
  mode (the live-server case), refusal of a non-empty directory, partial
  archive exit code when a tenant has no alias, manifest written last.
- Lock: a held `try_lock` makes `--restore` and `--backup --offline` fail;
  `--backup` proceeds.
- Restore: every refusal rule in §4 step 1 with its message and that no file
  under `store.dir` changed (hash the directory before and after); staged →
  renamed swap removes stale `-wal`/`-shm`; idempotent second run; `--user`
  leaves `control.db` untouched; `--reembed` creates an empty generation and
  re-arms exactly `embedded` jobs.
- `store_drift` unit tests for the three set shapes, and `heal_store_drift`
  still re-arms exactly what it did (existing tests keep passing).
- Verification: each row of §5 produced by a fixture that breaks exactly that
  invariant; exit codes 0/2/1.

**Against a real Qdrant (`tests/integration_qdrant.rs`, `#[ignore]`, run by
the `qdrant integration` CI job, `.github/workflows/ci.yml:73`):**

- export: snapshot created on the resolved generation, downloaded, hash
  equals a second local hash, server-side snapshot deleted afterwards,
  `points` equals `count()`.
- import: upload into `alias_g{N+1}`, `await_readable`, payload indexes
  present, `points/count` equals manifest, alias re-pointed, previous
  generation still exists, `all_artifact_ids()` on the alias equals the
  pre-backup set.
- refusal: a snapshot of a collection at a different `dim` is refused before
  the alias moves.
- round trip (slice 3): back up a two-tenant base, delete both collections
  and both `.db` files, restore, `--verify-stores` → `PASS`.

What the memory store cannot prove and only the Qdrant job does: that
Qdrant's snapshot format carries the collection config and payload indexes,
that `priority=snapshot` restores into a freshly named collection, and the
streaming upload size behaviour.

## 8. Interface summary

| flag | reads | writes | needs server stopped |
|---|---|---|---|
| `--backup DIR [--offline]` | config, control + tenant DBs, Qdrant | `DIR` | only with `--offline` |
| `--backup-check DIR` | `DIR` | nothing | no |
| `--restore DIR [--user S] [--reembed] [--force-version]` | `DIR`, config | control + tenant DBs, Qdrant | yes (lock) |
| `--verify-stores [--user S]` | config, DBs, Qdrant | nothing | no |

All four are flags on the existing `Args` in `src/main.rs`, dispatched where
`--reindex` and `--recompute-coverage` are, before the server starts. No HTTP
route, no new port, no new crate. The archive lives wherever the operator
points `DIR`; the README recommends outside `store.dir` and says that
shipping it off host, and encrypting it, are the operator's job today.

## Open questions (none block the plan; recommendation first)

1. **Prune orphan points at restore?** Recommend yes: the database is the
   authority (`--delete-user` and `delete_artifact` both treat it so), and a
   point with no row is unreachable content that only `heal_store_drift`'s
   warning will ever mention. Alternative: report only, as the sweep does.
2. **Directory, not tarball?** Recommend directory (no crate, streamable,
   `tar` is one command away). Alternative: write a `.tar` with a crate.
3. **Adopt the lock in `--delete-user` / `--reindex`?** Recommend a
   follow-up issue; both have the same hazard today and it is out of scope.
4. **Version skew is weak** while `CARGO_PKG_VERSION` is a constant `0.1.0`.
   Recommend a `build.rs` that exports the git sha into the manifest and the
   `/version` body (`src/web/app.rs:132`) as a follow-up; v1 compares the
   semver and relies on `--force-version`.
5. **Encryption at rest** of the archive: follow-up only, per the issue. A
   note in the README that `age` over the directory is the interim answer.

## Rollback

Each slice is additive (new flags, two defaulted trait methods, one lock
file) and reverts cleanly. A restore's own rollback is the previous
generation `--restore` leaves in Qdrant plus the operator's archive; the
report prints the exact collection name to re-point to and the one to delete.
