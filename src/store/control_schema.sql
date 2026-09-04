-- The control plane: who exists, and what work is queued for them.
--
-- Separate from `schema.sql` because these tables are about people and
-- scheduling rather than knowledge. Every knowledge table lives in a
-- per-tenant database that never learns other tenants exist, which is what
-- makes isolation structural rather than a predicate: there is no query
-- anywhere that could be written without a tenant filter, because below this
-- file no tenant filter exists.

CREATE TABLE IF NOT EXISTS users (
  subject      TEXT PRIMARY KEY,
  email        TEXT,
  -- Filesystem- and collection-safe tenant key. Derived once from `subject`
  -- and stored, not recomputed: an OIDC subject may contain anything at all,
  -- an email can change, and the mapping has to survive a later change to how
  -- the derivation works.
  slug         TEXT NOT NULL UNIQUE,
  -- Whether this user may apply tuning recommendations on /ui/insights — the
  -- only route in the tree that writes config.toml. Granted out of band with
  -- `engram --grant-judge`; there is no role model behind it and no page that
  -- sets it.
  can_judge    INTEGER NOT NULL DEFAULT 0,
  -- Where a due reminder is pushed, namespaced JSON: {"gotify": {"url",
  -- "token"}, "unifiedpush": {"endpoint"}}. '{}' means nowhere, and the
  -- Remind unit is never armed for this user.
  notify       TEXT NOT NULL DEFAULT '{}',
  -- Which of the ten languages this account's captures are read in. '' means
  -- automatic: the browser's Accept-Language decides, per capture, which is
  -- what an account that has never opened Settings gets. The resolved value is
  -- stamped onto each corpus at capture — see `Capture::with_lang` — because a
  -- background job holds a cached `Core` that knows no subject and could not
  -- read this column when it matters.
  lang         TEXT NOT NULL DEFAULT '',
  created_at   INTEGER NOT NULL
);

-- ── Auth ─────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS sessions (
  id         TEXT PRIMARY KEY,
  subject    TEXT NOT NULL,
  email      TEXT,
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_expiry ON sessions(expires_at);

CREATE TABLE IF NOT EXISTS api_tokens (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  token_hash   TEXT NOT NULL,
  subject      TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at   INTEGER,
  -- What asked for the token, as it announced itself. The extension mints
  -- every one of its tokens under the same name, so without this two rows can
  -- be identical in everything a person can read.
  user_agent   TEXT
);

-- ── Queue ───────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS jobs (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  -- Whose work this is.
  --
  -- The queue is instance-wide because the inference endpoints are:
  -- `server.workers` is the admission point in front of one GPU, and it has to
  -- stay one number however many people sign up. A pool per user would let ten
  -- signed-in users fire ten times that many concurrent requests at a single
  -- endpoint, where throughput does not scale but collapses, and the queueing
  -- moves somewhere nobody can see it.
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
  -- Position within the batch of units armed together: the window index, the
  -- judge pair's index, the embed batch number. Zero for singletons. Claiming
  -- orders by it, so every document's first window runs before any document's
  -- second — which is what stops a large ingest starving a small one behind it.
  seq         INTEGER NOT NULL DEFAULT 0,
  -- Is someone waiting on this? 0 = foreground (the capture pipeline the
  -- operator is watching move raw → ready), 1 = background (work nobody is
  -- standing in front of). One distinction and not a scale: a priority the
  -- operator can set wrong presents as "the capture is hanging", with nothing
  -- anywhere saying why. Default 0 because a row written before this column
  -- existed is foreground, which is the safe direction to be wrong in; the
  -- backfill in `migrate` puts the sweeps where they belong.
  class       INTEGER NOT NULL DEFAULT 0,
  -- How many consecutive runs of this periodic unit found nothing to do. Read
  -- by `rearm_periodic` to widen the wait, reset by any run that did work and
  -- by `arm_now`, so new data cancels a backoff without anything having to
  -- remember it was in one.
  empty_runs  INTEGER NOT NULL DEFAULT 0,
  UNIQUE(subject, stage, target_id)
);
-- Ready work in the order `claim_job` takes it: what someone is waiting on
-- first, then least-tried, then oldest.
--
-- The column order is the query's, not the filter's. `run_after` last looks
-- wrong until you try it the other way round: an inequality ends an index's
-- usable ordering, so `(state, run_after, attempts, id)` finds the ready rows
-- and then sorts them in a temp B-tree on every poll. This walks `state`,
-- `class`, `attempts`, `id` in claim order, tests `run_after` on each entry,
-- and stops at the first row that is ready — covering, and no sort. `class`
-- leads for the same reason `run_after` trails: it is an equality the walk can
-- carry, so priority costs the hot path nothing. Ageing is a written column
-- rather than a computed age precisely so that stays true — see
-- `Store::age_background`.
--
-- Superseded index dropped by name: `CREATE INDEX IF NOT EXISTS` leaves an
-- existing index's columns alone, so a base that already had `idx_jobs_claim2`
-- would keep claiming through the old order — silently, and on exactly the
-- installs the new one exists to serve.
DROP INDEX IF EXISTS idx_jobs_claim2;
CREATE INDEX IF NOT EXISTS idx_jobs_claim3  ON jobs(state, class, attempts, seq, id, run_after);
CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at);
-- Ageing, which asks one tenant at a time. `idx_jobs_claim3` leads on `state`
-- and cannot narrow to a subject at all, so without this the hourly pass walks
-- every waiting background unit on the instance once per registered user.
CREATE INDEX IF NOT EXISTS idx_jobs_age ON jobs(subject, state, class);
