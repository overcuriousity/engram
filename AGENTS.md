# AGENTS.md

engram: a self-hosted knowledge base searched by meaning, for one person or a small group, over the web UI, a REST API, MCP, a CLI, a browser extension and an Android app.

## Rules

This app follows the overcuriousity dev manifest **v2026.11.1**:
https://github.com/overcuriousity/overcuriousity-dev-manifest/tree/v2026.11.1/manifest

- Profile: **everyday**.
- Read the manifest files in number order before changing UI, security or ops.
- Before declaring work done, run `manifest/99-review-checklist.md` and `oc-check .`.

## Layout

- `src/web/` routes, templates (`templates/`), the security layer (`security/`), live updates, prefs, Trash.
- `src/core/` capture, search, ask, curation; `src/jobs/` the queue's stages; `src/infer/` model endpoints.
- `src/store/` the per-tenant SQLite store and the control database, each with its own migrator.
- `src/vector/` Qdrant, SQLite (contained) and in-memory vector stores behind one trait.
- `src/auth/` sessions, tokens, OIDC; `src/mcp/` the MCP server; `src/cli/` the client half of the binary.
- `assets/` CSS layers (`00`–`65` from the kit, `70`–`78` engram's), JS modules, fonts, icons, vendored htmx.
- `extension/` browser extension; `android/` the app; `deploy/` systemd unit and nginx; `scripts/` release and install.

## Commands

- `cargo test` and `cargo test --features contained --lib`
- `cargo clippy --all-targets -- -D warnings` and `cargo clippy --all-targets --features contained -- -D warnings`
- `cargo deny check`
- after changing a query or a schema: `cargo run -- --dev-schema target/sqlx-prepare.db`, then `DATABASE_URL=sqlite://target/sqlx-prepare.db cargo sqlx prepare -- --all-targets --features contained`; commit `.sqlx/`
- `oc-check .` on a source checkout (a build writes `assets/app.css`, which holds the tokens; delete it first)
- `node --test assets/js/*.test.mjs`
- `cargo test --lib -- --ignored` and `cargo test --test browser_ask -- --ignored` with `ENGRAM_CHROME` set (layout at 390–2560 px)
- `sh scripts/test-install.sh` after `cargo build`; `shellcheck install.sh build-lowmem.sh extension/pack.sh scripts/*.sh`

## Deviations from the manifest

| Rule | Deviation | Reason |
|---|---|---|
| SQLite only | Qdrant beside SQLite | integral to engram's retrieval; SQLite vectors serve the contained build |
| `unsafe_code = "forbid"` | `deny`, allowed only in `src/infer/whisper.rs` and `register()` in `src/vector/sqlite.rs` | in-process inference needs FFI; `forbid` cannot be overridden |
| sqlx migrations | `Store::migrate` / `Control::migrate` | they read a base's columns before touching it; live bases depend on it |
| compile-time-checked queries | runtime `sqlx::query` in the files `NOT_YET_CHECKED` lists (`tests/csp_static.rs`) | conversion in progress; the list only shrinks |
| utoipa OpenAPI + parity test | none | owner's decision |
| passkeys | none | owner's decision |
| `oc-kit` dependency | ported into engram's own modules | owner's decision; `oc-check` is a dev-dependency only |
| musl for both targets | aarch64 is `aarch64-unknown-linux-gnu` | bundled SQLite and the C dependencies were not tried under cargo-zigbuild |
| no decorative motion | the vector background (`[ui.background]`) stays | owner's decision; it passes the CSP and contrast rules and is static under `prefers-reduced-motion` |
| deployment keys top level, app settings under `[app]` | `public_url`, `trusted_proxies`, `allow_plain_http` under `[server]`; existing sections stay | restructuring breaks live configs; **to be streamlined later** (ROADMAP) |
| origin check on every state-changing request | a bearer request without a cookie skips it | the extension posts from its own origin; not verified in a browser here |
| deliberate tagged releases | every green push to master is released as `YYYY.MMDD.N` | owner's decision |
| tower-sessions-sqlx-store 0.15 | 0.16 | 0.15 needs sqlx 0.8; engram is on sqlx 0.9 |
| sessions per subject | deleting a user signs every browser out | tower-sessions records are opaque to SQL |
