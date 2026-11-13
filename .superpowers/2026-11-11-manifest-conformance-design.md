# engram: conformance with the overcuriousity dev manifest

Date: 2026-11-11. Manifest version followed: **v2026.11.1**
(https://github.com/overcuriousity/overcuriousity-dev-manifest/tree/v2026.11.1/manifest).
Committed with its plan by the owner's decision (2026-11-11), against the manifest's rule on agent working files.

## Intent

engram predates the manifest and has never claimed to follow it. The owner
wants engram to be consistent with their other apps and to look better. Every
manifest rule is applied wherever it fits engram's concept, technically and
visually. What bothers the owner most today is how prose-heavy engram is, on
its pages and in its source.

## Goals

- engram follows manifest v2026.11.1, profile **everyday**, and says so in a
  new `AGENTS.md`, with every deviation listed there with a one-line reason.
- Pages, the extension panel and the Android app say few words: a label, a
  unit, a range, a short gloss. No explanatory sentences.
- Source comments follow `80-code.md`: only where the code is not
  self-explanatory, saying why, never longer than the code they explain.
- The manifest's security layer is in place.
- `oc-check .` passes and runs in CI.

## Non-goals

- No user-facing documentation is written to replace removed text. Removed
  explanations are deleted. The code is expected to explain itself through
  names and structure, not through docstrings.
- No OpenAPI/utoipa. No passkeys. No dependency on `oc-kit`.
- No sqlx migration files; engram's own migrator stays.
- No change to what engram does: capture, search, ask, curation, tuning and
  the jobs behave as before. This is a conformance pass, not a feature pass.

## Anti-goals

- Meaning lost in the comment cut: a comment that records a non-obvious
  constraint (an RFC rule, a measured threshold, a past bug the code guards
  against) is shortened to one or two lines, not deleted. Where a long
  comment existed because the code was unclear, the code is renamed or
  restructured so the comment is unnecessary.
- Old behaviour kept alive beside new behaviour "for compatibility" (old
  token format, old cookie, native confirms). One way to do each thing.
- Text-guessing heuristics as fixes anywhere (owner's standing rule).
- A borrowed label where content should show (owner's standing rule:
  nameless artifacts show their own text).

## Constraints

- One feature branch off `master`, one PR. Commits follow the five stages
  below, in order, so the PR can be reviewed commit by commit.
- A live deployment exists with tenant data. The custom migrator and its
  column-reading behaviour are untouched in substance. Any schema addition
  (Trash, token id, sessions table) goes through that migrator.
- Rust edition 2024, `rust-version` stays as declared. Qdrant remains a
  supported, integral vector backend beside SQLite.
- Targeted tests gate commits; one full-suite run in the background at the
  end, reported after. Device testing for Android is batched at the very end.
- Never `git stash` in this repo.
- Commits end with `Co-Authored-By: Claude Fable 5.2 <noreply@anthropic.com>`.

## Decided deviations (go into AGENTS.md)

| Rule | Deviation | Reason |
|---|---|---|
| SQLite only | Qdrant beside SQLite | integral to engram's retrieval |
| `unsafe_code = "forbid"` | allowed only in code behind the `contained` feature and the FFI it needs (llama.cpp, whisper.cpp, sqlite-vec) | in-process inference cannot exist without FFI; everything else forbids it |
| sqlx migrations | engram's `Store::migrate` / `Control::migrate` | they read a base's columns before touching it; live bases depend on it |
| utoipa OpenAPI + parity test | none | not applicable by owner's decision |
| passkeys | none | not applicable by owner's decision |
| musl for both targets | builder to check; aarch64 is gnu today | see "left to the builder" |
| no decorative motion | the vector background (`[ui.background]`, `src/web/vbg.rs`, `05-background.css`) stays | owner's decision; it must still pass the CSP and contrast rules and stop under `prefers-reduced-motion` |
| top-level deployment keys, app settings under `[app]` | `public_url`, `trusted_proxies`, `allow_plain_http` live under `[server]`; the existing top-level sections stay | restructuring would break live configs; **to be streamlined later** — say so in `AGENTS.md` and add a ROADMAP entry |
| origin check on every state-changing request | requests carrying a bearer token and no cookie skip it | the extension posts from its own origin (designer's call; builder verifies the need in a browser first and drops the exemption if captures pass without it) |
| deliberate tagged releases | continuous release on green master stays | owner's decision; see stage 5 |

## Stage 1: words

- Every template under `src/web/templates/` is reduced to labels and short
  glosses per `00-principles.md` ("few words") and `40-interaction.md`
  (English, sentence case, verbs on buttons, actions named by effect, errors
  one sentence, no exclamation marks or emoji, "Sign in"/"Sign out").
- Introductory and explanatory paragraphs on pages (settings, corpus,
  insights, tune, housekeeping, pairing, extension install) are deleted.
- `src/web/ui_error.rs` and other user-facing error strings: one actionable
  sentence each.
- `config.example.toml`: at most one short line per key (builder's
  judgement which keys need one). It stays the authoritative config
  reference; the README keeps pointing at it.
- README stays, shortened where it narrates; it must keep naming every
  client.
- The 13 files under `docs/superpowers/` are removed from the tree.
  `docs/api.md` and `docs/evaluation.md` are reference documentation and stay.

## Stage 2: security layer

Ported from `crates/oc-kit/src/security/` and `auth/` as reference, not as a
dependency.

- Exact CSP from `60-security.md` on every HTML response, plus `nosniff`,
  `Referrer-Policy`, COOP, HSTS when `public_url` is https, `no-store` on
  authenticated responses (engram already sets `private, no-store` on HTML).
- All inline code removed: the theme script in `layout.html` becomes a
  blocking external `theme.js`; all `style=` attributes become classes; all
  `on*=` handlers and `hx-confirm` go; the htmx trigger filter in
  `_idle_foot.html` is replaced by something that needs no eval; dynamic
  sizes go through the CSSOM.
- htmx runs with the manifest's exact config. Inline SVG (pairing QR, icons)
  must still pass the CSP; builder verifies.
- Origin check on state-changing requests and the Host check (421), as
  specified. `X-Forwarded-*` trusted only from `trusted_proxies`.
- Config gains `public_url`, `trusted_proxies`, `allow_plain_http` under
  `[server]` with the manifest's semantics. `ENGRAM__` env overrides stay;
  secrets may also come from `*_FILE` / `file://`.
- `public_url` becomes the single source for what it covers. Removed:
  `auth.oidc.redirect_url` (derived from `public_url`), the MCP public host
  derived from that redirect URL, the `X-Forwarded-Proto` scheme guess in
  `src/web/pair.rs`, and the `--i-know-this-is-insecure` flag (replaced by
  `allow_plain_http`). A removed key found in a config is refused at startup
  with one sentence naming its replacement, as engram already does for
  retired keys. Announced once with the token break.
- The bookmarklet is dropped as a client: its section on the extension page,
  its token minting and any route only it used. The README stops naming it.
- OIDC users are keyed on issuer and subject: an issuer column is added
  through the migrator and backfilled from the configured issuer.
- The staged-photo preview uses a `data:` URL instead of `blob:`.
- Sessions move to tower-sessions with its SQLite store, in the control
  database. Cookie `__Host-engram-session`, HttpOnly, Secure, SameSite=Lax,
  id rotated on sign-in; over allowed plain http the cookie is
  `engram-session` without Secure. Everyone is signed out once.
- Rate limits on sign-in and API routes per client address; existing body
  limits stay.
- API tokens become `<prefix>_<id>.<secret>`, argon2-hashed, shown once,
  looked up by id. **Clean break:** all existing tokens stop working at the
  upgrade; there is no old-format path. This covers the extension, the
  paired phone (pair grants / pairing flow issue the new format), MCP and
  scripts. The upgrade says so once, plainly (startup log and the changelog).
- MCP: with OIDC configured, `/mcp` is a resource server with
  `/.well-known/oauth-protected-resource`; otherwise app tokens. No built-in
  authorization server.
- OIDC identities match on issuer and subject, never e-mail (builder
  verifies current behaviour).
- A test checks rendered pages against the CSP rules using
  `oc_check::check_html_for`.

## Stage 3: look, components, interaction

Reference: `crates/oc-kit/assets/` and `crates/oc-kit/src/ui/`, and the
demo's everyday half (notes).

- Tokens: adopt `00-tokens.css` values including `border-control`, spacing
  scale, radii, z-index scale, `--shadow-float`, chart tokens if charts are
  drawn. engram's `--color-due` survives only if it can be expressed with a
  manifest token; otherwise builder proposes and flags it.
- Type: Atkinson Hyperlegible Next replaces Inter (server and extension);
  JetBrains Mono stays. Type roles from `10-visual.md`. Root font size a
  percentage; compact density (87.5 %) toggle in the top bar; no text below
  12 px. Theme and density stored per user, rendered server-side on `<html>`.
- A contrast test over every documented pair in both themes.
- Icons: Lucide, inline SVG, fixed vocabulary; replace engram's own icon
  partials and the `⚠`/`✓` text glyphs used as icons with the status-symbol
  pattern (symbol + colour).
- Components per `20-components.md`: links vs buttons (no text-only
  buttons, one primary per view), one focus ring, tables, notices (no boxes,
  tints or left stripes), facts lists, one-line empty states, forms with
  labels above, select/listbox upgrade, `<dialog>` / bottom sheet, copy
  buttons on values worth copying, keyboard shortcuts with `?`, `/`, Esc.
- Layout, everyday profile, per `30-layout.md`: bottom tab bar below 48rem
  (3–5 labelled destinations, primary action bottom-right), top navigation
  and a centred column of at most 46rem from 48rem, list and detail side by
  side from 90rem, third region from 150rem. Container queries in rem.
  Touch rules. PWA with an offline page; app data not cached on the device.
- Anti-patterns in `90-anti-patterns.md` removed where present (cards with
  shadows, pill badges on everything, stat tiles, gradients).
- Timing: feedback within 100 ms, spinner after 400 ms. Live updates over
  SSE instead of polling where engram polls today (`every 3s` idle foot).
- Destructive actions, as decided:
  - Delete artifact: soft delete at once, undo notice ("Deleted 'X' · Undo",
    6 s), item in Trash for 30 days, then purged. "Deprecate" keeps its
    existing meaning beside it.
  - Revoke token: at once, with Undo.
  - Re-read a passage, re-read a photo: run at once, no question.
  - Wipe recorded searches, questions and verdicts: typed-name dialog.
  - Delete a source and all its artifacts (page, `DELETE
    /api/v1/corpora/{id}` caller on Android): irreversible, typed-name
    dialog. No soft delete for sources.
  - No native `confirm()`, `alert()`, `prompt()` anywhere.
- Extension panel (`extension/shared/`): same tokens, fonts, components,
  short copy, no inline code.
- Android app: the parts that translate — palette for both themes, Atkinson
  and JetBrains Mono, few words, sentence case, Undo over confirmation
  dialogs, 44 px (48 dp) targets, labelled bottom navigation, status as
  symbol + colour. Standing rules hold: judging stays behind Settings and is
  never badged; offline says "server unreachable" plainly.

## Stage 4: code

- Lint table from `templates/lints.toml` in `Cargo.toml`: `unsafe_code`
  forbidden except under the `contained` feature (mechanism is the
  builder's: crate-level `forbid` cannot be overridden, so e.g. `deny` at the
  root with scoped `allow` in the FFI modules), `unwrap_used` and
  `expect_used` denied outside tests. The ~255 non-test `unwrap`/`expect`
  calls are replaced with real error handling.
- One `thiserror` error type with an HTTP status mapping (exists in
  `src/error.rs`; `anyhow` use in library code is folded into it where it
  crosses a module boundary — builder's judgement for the CLI).
- All product and test queries become compile-time-checked sqlx macros.
  The reference database is produced by running engram's own migrators on an
  empty file (control and tenant schemas together, or one per schema if
  names clash — builder decides and documents the prepare command in
  `AGENTS.md`). `.sqlx/` is committed; CI builds with `SQLX_OFFLINE=true`
  and checks `.sqlx/` is current. The migrator's own statements, the `vec0`
  vector-table queries, the one `format!`-built statement and any other
  genuinely dynamic SQL stay runtime, each with a one-line why.
- `tracing` with authorization and cookie headers redacted; no secrets in
  logs.
- An optional subsystem that is not configured shows no UI entry point
  (push, OIDC, speech, PDF, contained).
- Test names read as sentences; tests use real SQLite.
- The comment pass over all of `src/`, `build.rs`, `Cargo.toml`, workflows,
  CSS, JS, templates, extension and Android sources, per the anti-goal above.
- `rustfmt.toml`, `rust-toolchain.toml` from the templates.

## Stage 5: repo and releases

- Add: `LICENSE-MIT`, `LICENSE-APACHE` (engram is **MIT OR Apache-2.0**),
  `license` field in `Cargo.toml`, `CHANGELOG.md` (Keep a Changelog),
  `SECURITY.md`, `AIRGAP.md`, `THIRD-PARTY-LICENSES` (cargo-about with the
  templates' `about.toml`/`about.hbs`), `deny.toml`, `AGENTS.md`.
- `cargo-deny` replaces `rustsec/audit-check` in CI: advisories, licences,
  bans, sources. Allowed licences as in the manifest. The git source for the
  patched `docling` crates is allow-listed. The existing ignore for
  RUSTSEC-2026-0459 moves into `deny.toml` with its reachability argument.
  If any dependency fails the licence policy, stop and report it to the
  owner; do not swap dependencies silently.
- CI gains `oc-check`, shellcheck, and keeps fmt, clippy `-D warnings`,
  tests, MSRV, the contained job, Qdrant integration, browser layout,
  Android. All actions pinned to full commit SHAs with minimal
  `permissions:`. Dependabot stays weekly and grouped.
- Release stays continuous: every green push to master is released with
  the computed `YYYY.MMDD.N` version. Changes are written under
  `## [Unreleased]`; the release workflow turns that heading into the
  computed version in the released tree, so tag, Cargo version and CHANGELOG
  heading agree by construction (builder decides whether the workflow
  commits that back to master or only stamps the release checkout).
- Release contents: offline
  bundle per target (binary, `install.sh`, systemd unit, nginx template,
  example config, licences, `SHA256SUMS`) plus a vendored source archive;
  build provenance attested. The Android APK stays part of the release.
- `install.sh` gains `--archive <bundle>` (no network, checksum verified,
  `~/.local/bin` by default, `--system` for root, never sudo), keeping the
  online path.
- `engram.service` becomes the hardened unit from
  `templates/deploy/app.service` (`Type=notify` with watchdog, so engram
  gains `sd_notify` support); an nginx reference config is added under
  `deploy/`.
- Versioning stays CalVer in the tag scheme already in use (`v2026.919.1`).

## Left to the builder

- Order of work inside each stage and how commits are cut within it.
- How the kit's CSS/JS/security code is ported into engram's module layout.
- Whether aarch64 can move to musl given bundled SQLite and the contained
  build; if not, record it as a deviation with the reason.
- Exact SSE design for what polls today.
- Which destinations the phone tab bar and top navigation carry, starting
  from the current ones.
- Where Trash is reached from (Settings is the default assumption).

## Calls made by the designer, not the owner

- Port from `oc-kit`, do not depend on it; `oc-check` only as CI lint and
  dev-dependency.
- `config.example.toml` cut to one line per key.
- tower-sessions store lives in the control database.
- `docs/api.md` and `docs/evaluation.md` count as reference docs and stay.
- cargo-deny replaces audit-check rather than running beside it.
- Online `install.sh` path kept beside `--archive`.
- Bearer-without-cookie requests skip the origin check (the owner dropped
  the bookmarklet; the exemption for the extension is the designer's).
- Removed config keys are refused at startup rather than ignored.
- OIDC issuer column backfilled from the configured issuer; photo preview
  as `data:` URL; `sd_notify` added for the hardened unit.

## Done when

- `manifest/99-review-checklist.md` passes, `oc-check .` is clean.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` (default and
  `contained`), `cargo test`, `cargo deny check` are green.
- Phone (390 px), desktop and ultrawide checked in the browser tests.
- The offline bundle builds and installs without network.
- The PR description lists the deviations, the clean break on tokens and
  sessions, and what was verified.
