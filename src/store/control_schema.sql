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
  -- Whether this user may reach /ui/judge, which is also the only route in the
  -- tree that writes config.toml. Granted out of band with
  -- `engram --grant-judge`; there is no role model behind it and no page that
  -- sets it.
  can_judge    INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL
);
