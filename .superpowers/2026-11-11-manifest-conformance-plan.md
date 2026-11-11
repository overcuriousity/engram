# engram Manifest Conformance Implementation Plan

> **For agentic workers:** implement this plan task by task, in order, on the branch `feat/manifest-conformance`, which already exists on the remote. If the superpowers skills are available, use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans; otherwise work through the checkboxes directly. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> You have no access to the session that wrote this plan, to the owner's machine, or to the live deployment. Everything you need is in this file, in the design beside it, in the repository and in the manifest repository. Where a step needs something you do not have (a browser, a phone, a container runtime for Qdrant), do the rest, and list what was skipped in the PR description instead of guessing a result.

**Goal:** Bring engram (server, browser extension, Android app) in line with the overcuriousity dev manifest v2026.11.1, profile everyday, with the deviations the owner decided.

**Architecture:** engram ports code from the manifest's reference kit into its own modules and does not depend on it. A new `src/web/security/` module wraps the router; the kit's CSS layers, JS modules, fonts and icons replace engram's front-end foundation; pages are then rebuilt on the kit's components. Schema additions go through engram's own migrators.

**Tech Stack:** Rust 2024, axum 0.8, Askama, htmx 2.0.11 + SSE extension, sqlx (SQLite, compile-time-checked), tower-sessions, Qdrant, Kotlin/Compose (Android), WebExtension.

**Spec:** `.superpowers/2026-11-11-manifest-conformance-design.md`. Read it first; it holds the reasons and the deviation table.

**Reference (read-only, never build or run it):** a clone of the manifest at tag v2026.11.1. Clone it yourself with `git clone --branch v2026.11.1 --depth 1 https://github.com/overcuriousity/overcuriousity-dev-manifest <dir>` into a scratch directory outside the repo. Below, `KIT/` means `<dir>/crates/oc-kit/`, `RULES/` means `<dir>/manifest/`, `TPL/` means `<dir>/templates/`, `DEMO/` means `<dir>/crates/demo/`.

## Global Constraints

- One branch `feat/manifest-conformance` off `master`; one PR at the end. Never `git stash`. The design and this plan under `.superpowers/` are committed by the owner's decision; commit nothing else from there, and nothing under `.claude/`.
- Commits follow the task order. Every commit ends with `Co-Authored-By: Claude Fable 5.2 <noreply@anthropic.com>`.
- Targeted tests gate each commit (`cargo test <filter>`). The full suite runs once, in the background, in Task 27.
- UI copy: English, sentence case, a label or a short gloss, never an explanatory sentence; verbs on buttons; errors one sentence; no exclamation marks, no emoji; "Sign in" / "Sign out".
- Comments: only where code is not self-explanatory, say why, never longer than the code. New code written in any task already follows this.
- No text-guessing heuristics as fixes. Nameless artifacts show their own text, never a parent's name.
- No behaviour change to capture, search, ask, curation, tuning or jobs.
- No old path kept beside a new one (tokens, cookie, confirms, config keys).
- Qdrant stays. engram's migrators stay. No utoipa, no passkeys, no `oc-kit` dependency.
- If `cargo deny check licenses` fails on a dependency: stop and report to the owner; do not swap the dependency.
- `rust-version` in `Cargo.toml` is not raised.

## Review Focus

1. A live control database and tenant base from before this branch boot on the new binary: migration succeeds, old tokens and sessions are gone, data is intact. (Task 9 test.)
2. A config file that still carries a removed key (`auth.oidc.redirect_url`) is refused with one sentence naming `server.public_url`; it is not silently ignored. (Task 6 test.)
3. The extension's capture POST, sent with `Origin: moz-extension://…` and a bearer token, is accepted; the same request with a session cookie and a foreign origin is rejected. (Task 7 test.)
4. An artifact in Trash never appears in search, ask, due, day or lineage, on both vector backends, and comes back complete on restore. (Task 11 test; Qdrant half in `tests/integration_qdrant.rs`.)
5. Every rendered page passes the CSP check with real content in it, including markdown bodies, the pairing QR and the vector background. (Task 10 test.)

---

## Stage 1 — Words

### Task 1: Branch, and remove committed specs

**Files:** Delete: `docs/superpowers/` (13 files).

- [ ] **Step 1:** `git switch feat/manifest-conformance` (it exists on the remote and already carries this plan and the design).
- [ ] **Step 2:** `git rm -r docs/superpowers`
- [ ] **Step 3:** Run `git ls-files docs` — expected: `docs/api.md`, `docs/evaluation.md` only.
- [ ] **Step 4:** Commit `chore: remove committed specs and plans`.

### Task 2: Cut page copy

**Files:** Modify: every file in `src/web/templates/`, `src/web/ui_error.rs`, user-facing strings in `src/web/*.rs` (grep for string literals rendered into pages), `extension/shared/panel.html`, `extension/shared/panel.js`, `extension/shared/pair.js`.

Rules for the cut (from `RULES/00-principles.md`, `RULES/40-interaction.md`):
- Delete every introductory or explanatory `<p>` on a page. Keep a `.muted` gloss only where a term would otherwise be unreadable, at most ~6 words.
- Buttons: verb, named by effect ("Delete source", "Revoke", "Read again").
- Errors: one sentence, what happened and what to do.
- Leave `confirm(...)` attributes in place for now (Tasks 8 and 11 remove them) but do not edit their text.
- Template comments (`{# … #}`) are cut in Task 21, not here.

- [ ] **Step 1:** Write test `tests/copy.rs::no_template_paragraph_is_longer_than_a_gloss`: walk `src/web/templates/*.html`, strip Askama tags and HTML tags and `{# #}` comments, and for each text node outside `<pre>`, `<code>` and markdown-body includes assert it has at most 12 words and contains no `!`. Allow-list by exact file+text only where content is user data, not copy.
- [ ] **Step 2:** Run `cargo test --test copy` — expected: FAIL listing offenders.
- [ ] **Step 3:** Cut the copy in every template until the test passes. Cut `ui_error.rs` messages to one sentence each.
- [ ] **Step 4:** Add test `tests/copy.rs::every_ui_error_is_one_sentence` over all variants rendered by `ui_error.rs` (one `.` at most, ≤ 20 words).
- [ ] **Step 5:** Run `cargo test --test copy` and `cargo test web::` — expected: PASS. Update assertions in existing tests that matched deleted text.
- [ ] **Step 6:** Commit `refactor(ui): cut page copy to labels and glosses`.

### Task 3: Cut config example and README

**Files:** Modify: `config.example.toml`, `cli.example.toml`, `README.md`, `android/README.md`, `extension/README.md`.

- [ ] **Step 1:** `config.example.toml`: every key keeps at most one comment line of ≤ 100 characters; section banners (`# ── … ──`) removed; multi-paragraph rationale deleted. Keys and values unchanged.
- [ ] **Step 2:** Run `cargo test config` — expected: PASS (the example must still parse; an existing test loads it — if none does, add `config::tests::the_example_file_loads`).
- [ ] **Step 3:** README: keep What it does, Requirements, Install, First run, Multiple users, Backup, Clients (every client named), Configuration (points at `config.example.toml`), Docs. Delete narrative passages; no section longer than it needs to state commands and facts.
- [ ] **Step 4:** Commit `docs: shorten the config example and READMEs`.

---

## Stage 2 — Security layer

### Task 4: Front-end foundation from the kit

Builder's-order call: the kit's tokens, JS modules and page head are ported here, before the CSP, because the CSP needs `theme.js` and the htmx config; pages are rebuilt on the components in Stage 3.

**Files:**
- Create: `assets/css/00-tokens.css` (replace with `KIT/assets/css/00-tokens.css`, plus engram's `--color-due` pair kept at the end under a one-line comment), `assets/css/10-base.css`, `20-layout.css`, `30-components.css`, `40-tables.css`, `50-touch.css`, `60-charts.css` and `profile-everyday.css` copied from `KIT/assets/css/` as files `10-oc-base.css` … `65-oc-profile-everyday.css` so they sort before engram's own layers; engram's existing layers renamed `70-…` to `79-…` in their current order.
- Create: `assets/js/` with `theme.js`, `prefs.js`, `notices.js`, `dialog.js`, `menu.js`, `keys.js`, `copy.js`, `busy.js`, `live.js`, `rows.js` and their `*.test.mjs` from `KIT/assets/js/`.
- Create: `assets/vendor/htmx.min.js` (2.0.11), `assets/vendor/htmx-ext-sse.js`, their licence files, from `KIT/assets/vendor/`. Delete `assets/htmx.min.js`.
- Create: `assets/fonts/atkinson-next-{400,500,600}.woff2`, `OFL-AtkinsonHyperlegibleNext.txt`, `OFL-JetBrainsMono.txt`. Delete `assets/fonts/inter-*.woff2`.
- Create: `assets/icons/*.svg` + `LICENSE-lucide.txt` from `KIT/assets/icons/`.
- Modify: `build.rs` (`build_stylesheet` unchanged in mechanism; `stamp_assets` hashes every file under `assets/js`, `assets/vendor`, `assets/app.css`), `src/web/assets.rs`, `src/web/templates/layout.html`.

**Interfaces — Produces:**
- `crate::web::assets::url(path: &str) -> String` — content-hashed URL `/assets/<path>?v=<hash of that file>`; served `Cache-Control: public, max-age=31536000, immutable`.
- `layout.html` head: `<meta name="htmx-config" content='…'>` with the exact JSON from `RULES/50-stack.md`; blocking `<script src="{{ url("js/theme.js") }}">`; htmx and sse ext `defer`; kit modules as `type="module"`; `app.js` after them. `<html>` carries `data-theme`, `data-density`, `data-prefs` (values come from Task 13; until then omit the first two and set `data-prefs="local"`).
- Body ends with `<div class="oc-notices" id="oc-notices" aria-live="polite">`, `<div class="oc-sr-only" id="oc-live" aria-live="polite">` and the `#oc-shortcuts` dialog as in `KIT/templates/oc/page.html`.

- [ ] **Step 1:** Write test `web::assets::tests::every_asset_url_is_content_hashed_and_immutable` (GET `url("js/theme.js")` → 200, `cache-control` contains `immutable`; changing the `v` still serves).
- [ ] **Step 2:** Run it — expected: FAIL (`url` not defined).
- [ ] **Step 3:** Copy the files, implement `url`, rewrite the layout head and tail. Remove the inline theme `<script>` from `layout.html`; `theme.js` reads `localStorage` key `oc.theme` — migrate the old key by reading `engram.theme` once in `app.js` and writing `oc.theme`, then deleting the old key.
- [ ] **Step 4:** Add `tests/contrast.rs` ported from `KIT/tests/contrast.rs`, reading `assets/css/00-tokens.css`, plus the `--color-due` pair on `bg-base` (≥ 4.5:1 both themes).
- [ ] **Step 5:** Run `cargo test --test contrast`, `cargo test web::assets`, `node --test 'assets/js/*.test.mjs'`, and `cargo test --test browser_ask` — expected: PASS. Fix selectors in `assets/app.js` that relied on htmx 2.0.4 behaviour only if a test shows it.
- [ ] **Step 6:** Commit `feat(ui): port the kit's tokens, type, icons and modules`.

### Task 5: Security headers and host check

**Files:**
- Create: `src/web/security/mod.rs`, `src/web/security/headers.rs`, `src/web/security/client_ip.rs` (port of `KIT/src/security/{mod,headers,client_ip}.rs`).
- Modify: `src/web/mod.rs` (wrap the router), `src/config.rs` (see Task 6 for keys; this task uses the `Deployment` type).
- Test: `src/web/security/tests.rs` (port the relevant cases of `KIT/tests/security.rs`).

**Interfaces — Produces:**
- `pub const CSP: &str` — the exact string in `RULES/60-security.md`.
- `pub struct Deployment { pub public_url: Option<url::Url>, pub trusted_proxies: Vec<ipnet::IpNet>, pub allow_plain_http: bool, pub bind: std::net::SocketAddr }` with `fn host_allowed(&self, host: &str) -> bool`, `fn origin_allowed(&self, origin: &str) -> bool`, `fn secure_cookies(&self) -> bool`, `fn base(&self) -> url::Url` (public URL, or `http://<bind>` on loopback).
- `pub struct ClientIp(pub std::net::IpAddr)` extractor honouring `X-Forwarded-For` only from `trusted_proxies`.
- `pub fn secure(router: Router, dep: &Deployment) -> Router` — applies headers, host check, origin check (Task 7 fills it), body limit stays where it is.

Tests (names are the spec):
- [ ] **Step 1:** Write `every_html_response_carries_the_exact_csp`, `nosniff_referrer_policy_and_coop_are_always_set`, `hsts_is_sent_only_when_public_url_is_https`, `referrer_policy_is_same_origin_over_plain_http`, `a_foreign_host_header_gets_421`, `loopback_hosts_pass_without_a_public_url`, `x_forwarded_host_is_ignored`, `x_forwarded_for_counts_only_from_a_trusted_proxy`.
- [ ] **Step 2:** Run `cargo test web::security` — expected: FAIL.
- [ ] **Step 3:** Port and wire. `/healthz` is exempt from the host check.
- [ ] **Step 4:** Run `cargo test web::security` — expected: PASS.
- [ ] **Step 5:** Commit `feat(security): response headers and the host check`.

### Task 6: Deployment config, removed keys

**Files:** Modify: `src/config.rs`, `src/main.rs`, `src/auth/local.rs`, `src/auth/oidc.rs`, `src/mcp/mod.rs`, `src/web/pair.rs`, `config.example.toml`, `README.md`.

**Interfaces — Produces:**
- `ServerConfig` gains `public_url: Option<String>`, `trusted_proxies: Vec<String>` (default `["127.0.0.1/32","::1/128"]`), `allow_plain_http: bool` (default false). `Config::deployment(&self) -> Result<Deployment>`.
- Validation: `public_url` required unless `bind` is loopback; an http `public_url` on a non-loopback host is refused unless `allow_plain_http`.
- Secrets: any key may be given as `ENGRAM__…__KEY_FILE=<path>`, and a value `file:///path` is read from that file (port `KIT/src/config.rs` behaviour and its tests in `KIT/tests/config.rs`).
- Removed: `auth.oidc.redirect_url` (now `deployment.base().join("auth/callback")`), `OidcClient::public_host` and its use in `src/mcp/mod.rs` (use `Deployment`), the `X-Forwarded-Proto` scheme guess in `src/web/pair.rs` (use `Deployment::base`), the `--i-know-this-is-insecure` flag and `assert_bind_is_safe`'s override parameter.
- Startup refusal, exact messages: `auth.oidc.redirect_url is gone; set server.public_url.` and, for the flag, clap's unknown-argument error is enough.

- [ ] **Step 1:** Write tests in `config::tests`: `a_public_url_is_required_off_loopback`, `plain_http_off_loopback_needs_allow_plain_http`, `a_config_with_redirect_url_is_refused_and_names_public_url`, `a_secret_can_come_from_a_file_env_or_a_file_url`, `the_oidc_callback_is_derived_from_public_url`.
- [ ] **Step 2:** Run `cargo test config::` — expected: FAIL.
- [ ] **Step 3:** Implement. Follow engram's existing pattern for retired keys (see how `[feedback] enabled` is refused).
- [ ] **Step 4:** Run `cargo test config:: auth:: mcp:: web::pair` — expected: PASS.
- [ ] **Step 5:** Add the three keys to `config.example.toml` under `[server]`, one line each. Commit `feat(config): public_url, trusted_proxies, allow_plain_http`.

### Task 7: Origin check and rate limits

**Files:** Create: `src/web/security/origin.rs`, `src/web/security/rate.rs` (ports of the kit's). Modify: `src/web/security/mod.rs`, `src/web/mod.rs`, `src/web/auth_routes.rs`, `src/web/api.rs`, `src/mcp/mod.rs`.

**Interfaces — Produces:**
- Origin rule for non-GET/HEAD/OPTIONS: a present `Origin` (not `null`) must satisfy `origin_allowed`; otherwise `Sec-Fetch-Site` must be `same-origin` or `none`; with neither header the request may carry no cookie. **Exemption (deviation):** a request with an `Authorization: Bearer` header and no `Cookie` header skips the check. Rejection is 403.
- `pub fn rate_limit(per_minute: u32, burst: u32) -> RateLayer` keyed on `ClientIp`, one bucket per IPv6 /64, bounded memory. Applied: `/auth/login` POST and `/pair/claim` at 10/min burst 5; `/api/v1` and `/mcp` at 600/min burst 120. Over the limit: 429 with `Retry-After`.

- [ ] **Step 0:** Verify the exemption is needed: load the unpacked extension in Firefox against a local engram with the check on and no exemption, capture a page, and record whether the POST carries `Origin: moz-extension://…`. If captures pass without the exemption, drop it and its row from the deviations list in Task 26. If no browser is available, keep the exemption and say so in the PR.
- [ ] **Step 1:** Write tests `a_same_origin_post_passes`, `a_foreign_origin_post_with_a_cookie_is_rejected`, `origin_null_counts_as_absent`, `a_post_with_no_origin_and_no_fetch_site_may_carry_no_cookie`, `a_bearer_request_without_a_cookie_skips_the_origin_check` (with `Origin: moz-extension://abc`), `a_bearer_request_with_a_cookie_does_not_skip_it`, `the_eleventh_sign_in_in_a_minute_gets_429`, `one_ipv6_64_shares_a_bucket`.
- [ ] **Step 2:** Run `cargo test web::security` — expected: FAIL.
- [ ] **Step 3:** Port and wire.
- [ ] **Step 4:** Run `cargo test web::security web::auth_routes web::api` — expected: PASS.
- [ ] **Step 5:** Commit `feat(security): origin check and rate limits`.

### Task 8: Remove inline code

**Files:** Modify: all templates with `style=`, `on*=`, `hx-confirm`, `hx-on`, trigger filters; `src/web/*.rs` that emit `style="`; `assets/css/7x-*.css`; `assets/app.js`; `src/web/vbg.rs` + `assets/css/…background.css`; `src/web/markdown.rs` (ammonia must strip `style`); `src/web/extension.rs` + `templates/extension.html` (drop the bookmarklet section and `/extension/phone` only if nothing but the bookmarklet uses it — check the Android pairing flow first); `README.md` (stop naming the bookmarklet).

Decisions:
- Each `style=` becomes a class in the matching CSS layer. Dynamic sizes (bars, the background canvas) are set from JS through `el.style.setProperty` (CSSOM is allowed by the CSP).
- `_artifact.html` edit toggle `onclick` → `data-toggle="#edit-<id>"` handled by one delegated listener in `app.js` using the `hidden` attribute.
- `_idle_foot.html` and `_queue.html` polling (`every 3s [...]`) → SSE (Task 12). In this task replace the eval filter with a plain `every 3s` and move the two conditions into the handler: the server answers 204 when there is nothing new (204 does not swap under the htmx config).
- The two re-reads (`_bands.html`, `corpus.html`): drop `onsubmit`; they run at once.
- The three remaining confirms (`settings.html` revoke and purge, `corpus.html`/source delete, `_artifact.html` `hx-confirm`) are replaced in Task 11; in this task remove the attribute and mark the form `data-pending-task-11` so the build stays honest. Task 11 removes the marker.
- Staged photo preview: `URL.createObjectURL` → `FileReader.readAsDataURL`.
- The vector background must stop under `prefers-reduced-motion: reduce` (static frame).

- [ ] **Step 1:** Write `tests/csp_static.rs::no_template_or_handler_emits_inline_code`: scan `src/web/templates/**` and `src/web/*.rs` string literals for `style="`, ` on[a-z]+="`, `<script>` without `src`, `hx-on`, `hx-confirm`, `hx-trigger="…[`, `javascript:`; scan `assets/app.js` and `assets/js/*.js` for `confirm(`, `alert(`, `prompt(` (the PWA install `p.prompt()` call is renamed via a local variable `installEvent.prompt()` — allow exactly that member call).
- [ ] **Step 2:** Run `cargo test --test csp_static` — expected: FAIL.
- [ ] **Step 3:** Do the removals above.
- [ ] **Step 4:** Run `cargo test --test csp_static`, `cargo test web::`, `cargo test --test browser_ask` — expected: PASS.
- [ ] **Step 5:** Commit `refactor(ui): no inline styles, scripts or handlers; drop the bookmarklet`.

### Task 9: tower-sessions, token format, OIDC issuer

**Files:** Modify: `Cargo.toml` (`tower-sessions = "0.15"`, `tower-sessions-sqlx-store = { version = "0.15.1", features = ["sqlite"] }`), `src/auth/mod.rs`, `src/auth/tokens.rs`, `src/auth/grants.rs`, `src/auth/oidc.rs`, `src/store/auth.rs`, `src/store/control.rs`, `src/store/control_schema.sql`, `src/web/auth_routes.rs`, `src/web/state.rs`, `src/web/mod.rs`, `src/web/app.rs`, `src/web/settings.rs`, `src/mcp/mod.rs`, `extension/shared/api.js` + `pair.js`, Android token handling (`android/core/src/**` — grep `engram_`).

**Interfaces — Produces:**
- `pub fn session_layer(control: &Control, dep: &Deployment) -> SessionManagerLayer<SqliteStore>`: cookie `__Host-engram-session` (HttpOnly, Secure, SameSite=Lax, Path=/) or `engram-session` without Secure when `!dep.secure_cookies()`; expiry on inactivity 30 days; `session.cycle_id()` on sign-in. Session key `"engram.subject"`. `SESSION_COOKIE`, `set_session_cookie`, `clear_session_cookie`, `cookie_value` and the `sessions` store functions are deleted.
- Control schema: drop table `sessions` (in the migrator's existing "column this schema no longer has" section, as a table drop); tower-sessions creates its own table via `SqliteStore::migrate()`.
- Tokens: plaintext `engram_<id>.<secret>` where `<id>` is the row id and `<secret>` is 32 random bytes base64url. `pub const TOKEN_PREFIX: &str = "engram_"`. `tokens::verify(control, presented) -> Result<Identity>` parses the id, loads that one row, argon2-verifies once. `api_tokens` gains `scheme INTEGER NOT NULL DEFAULT 1` (ADDITIVE); new rows write `2`. In `Control::migrate`, after ADDITIVE: `DELETE FROM api_tokens WHERE scheme = 1`, and if rows were deleted log once at `warn`: `"{n} API tokens from before this version were removed; mint new ones in Settings and pair clients again."`
- `tokens::revoke` sets `revoked_at`; new `tokens::restore(control, id, subject) -> Result<()>` clears it if revoked less than 30 days ago (used by Task 11).
- OIDC: `users` gains `issuer TEXT NOT NULL DEFAULT ''` (ADDITIVE). At startup with OIDC configured: `UPDATE users SET issuer = ?configured WHERE issuer = '' AND` the user is not a local user (builder: local users are those in `[auth.local]`). On callback: user found by subject must have matching issuer, else `Error::Forbidden`. Never match on e-mail (`is_allowed` may still use e-mail for the allow-list — check and, if it grants access by e-mail alone, restrict it to subject and groups).
- MCP: with OIDC configured, `GET /.well-known/oauth-protected-resource` returns `{"resource": "<base>/mcp", "authorization_servers": ["<issuer>"]}` and `/mcp` accepts IdP JWTs validated against JWKS (`jsonwebtoken = { version = "11", default-features = false, features = ["rust_crypto"] }`, algorithm taken from the key) as well as app tokens; without OIDC, app tokens only and no well-known route. Port from `KIT/src/auth/tokens.rs` and `identity/extract.rs`.

- [ ] **Step 1:** Write tests: `auth::tokens::tests::{a_minted_token_has_prefix_id_dot_secret, verifying_reads_one_row_by_id, a_token_without_an_id_is_unauthorized, a_revoked_token_can_be_restored_within_30_days}`; `store::control::tests::{migrating_an_old_control_db_removes_scheme_1_tokens_and_keeps_users, the_old_sessions_table_is_dropped}` (build the old schema by hand in the test from the pre-branch `control_schema.sql` text, insert a user, a token and a session, then migrate); `web::auth_routes::tests::{the_session_cookie_is_host_prefixed_secure_and_lax, the_cookie_loses_secure_and_prefix_over_allowed_plain_http, the_session_id_rotates_on_sign_in, an_oidc_user_from_another_issuer_is_refused}`; `mcp::tests::{the_protected_resource_document_points_at_the_issuer, there_is_no_well_known_route_without_oidc}`.
- [ ] **Step 2:** Run `cargo test auth:: store::control web::auth_routes mcp::` — expected: FAIL.
- [ ] **Step 3:** Implement. Extension and Android: nothing parses the token except to send it; verify with grep and fix any length or prefix check.
- [ ] **Step 4:** Run the same tests plus `cargo test --test multi_tenant` — expected: PASS.
- [ ] **Step 5:** Commit `feat(auth)!: tower-sessions, id-addressed tokens, issuer-bound OIDC users`.

### Task 10: Rendered pages against the CSP

**Files:** Modify: `Cargo.toml` (`[dev-dependencies] oc-check = { git = "https://github.com/overcuriousity/overcuriousity-dev-manifest", tag = "v2026.11.1" }`). Create: `tests/rendered.rs`.

- [ ] **Step 1:** Write `every_page_passes_the_csp_rules`: boot the app on in-memory SQLite with the memory vector store (see `src/web/test_support.rs`), seed one corpus, artifacts with markdown bodies (a table, a formula, a link to the app's own origin), one reminder, one token; sign in; GET `/`, `/ui/search?q=…`, `/ui/settings`, `/ui/insights`, `/ui/corpora/{id}`, `/ui/artifacts/{id}`, `/ui/day/today`, `/ui/pair`, `/ui/app`, `/extension/install`, `/auth/login` (signed out), the 404 page; for each assert `oc_check::check_html_for(&body, origin).is_empty()`, printing findings on failure.
- [ ] **Step 2:** Run `cargo test --test rendered` — expected: FAIL or PASS; fix every finding in the templates or renderers (not by allow-listing).
- [ ] **Step 3:** Commit `test: rendered pages pass the manifest's CSP rules`.

---

## Stage 3 — Look, components, interaction

### Task 11: Destructive actions

**Files:** Modify: `src/store/schema.sql`, `src/store/mod.rs` (ADDITIVE), `src/store/artifacts.rs`, `src/core/ingest.rs`, `src/web/artifact.rs`, `src/web/settings.rs`, `src/web/corpus.rs`, `src/web/api.rs`, `src/jobs/` (purger), templates `_artifact.html`, `settings.html`, `corpus.html`. Create: `src/web/templates/_notice_undo.html`, `_typed_confirm.html`, `trash.html` (port `KIT/templates/oc/{notice,typed-confirm}.html`, `DEMO/templates/notes/trash.html`), `src/web/trash.rs`.

**Interfaces — Produces:**
- Artifact lifecycle gains status `trashed`; `artifacts.trashed_at INTEGER` (nullable, ADDITIVE) and `artifacts.trashed_from TEXT` (the status it had). `Core::trash_artifact(&self, id: &str) -> Result<()>` sets both and calls `VectorStore::set_lifecycle` so retrieval excludes it; `Core::restore_artifact(&self, id: &str) -> Result<()>` reverses it; `Core::purge_trash(&self, older_than_secs: i64) -> Result<u64>` hard-deletes through the existing `delete_artifact`. `pub const TRASH_RETENTION_SECS: i64 = 30 * 24 * 3600; pub const UNDO_MS: u32 = 6000;`
- The idle pass (find where reaping runs in `src/jobs/`) calls `purge_trash(TRASH_RETENTION_SECS)` once per pass.
- Routes: `POST /ui/artifacts/{id}/delete` → trash, returns the undo notice out-of-band into `#oc-notices`: text `Deleted '<name or first words of the artifact's own text>' · Undo`. `POST /ui/artifacts/{id}/restore`. `GET /ui/trash` (linked from Settings) lists trashed artifacts with Restore. `DELETE /api/v1/artifacts/{id}` trashes; `POST /api/v1/artifacts/{id}/restore` added to `docs/api.md`.
- Token revoke: `POST /ui/ops/tokens/{id}/revoke` acts at once and returns `Revoked '<name>' · Undo`; `POST /ui/ops/tokens/{id}/restore`.
- Typed-name dialog (port `dialog.js` behaviour: the destructive button enables only when the input equals the name exactly): purge feedback — name to type `recorded searches`; delete source — the source's name. Button labels by effect: `Delete all recorded searches, questions and verdicts`, `Delete '<source>' and <n> artifacts`. Server side re-checks the typed name and answers 422 with an inline notice on mismatch.
- "Deprecate" and its undo are unchanged.

- [ ] **Step 1:** Write tests: `core::ingest::tests::{a_trashed_artifact_is_not_found_by_search, a_trashed_artifact_is_absent_from_due_day_and_lineage, a_restored_artifact_has_its_status_and_vectors_back, trash_older_than_30_days_is_purged_and_younger_is_kept}`; `vector::conformance::a_trashed_artifact_is_excluded_until_restored` (runs for memory, sqlite and, in `tests/integration_qdrant.rs`, Qdrant); `web::artifact::tests::deleting_answers_with_an_undo_notice_and_no_dialog`; `web::settings::tests::{revoking_a_token_can_be_undone, purging_needs_the_exact_typed_name}`; `web::corpus::tests::deleting_a_source_needs_its_name_typed`.
- [ ] **Step 2:** Run those — expected: FAIL.
- [ ] **Step 3:** Implement; remove every `data-pending-task-11` marker.
- [ ] **Step 4:** Run those plus `cargo test --test csp_static --test rendered` — expected: PASS.
- [ ] **Step 5:** Commit `feat(ui): undo and trash for artifacts and tokens, typed-name dialogs for the rest`.

### Task 12: Live updates over SSE

**Files:** Create: `src/web/live.rs` (port `KIT/src/sse.rs`: `Broadcaster<T>`, `sse_response`). Modify: `src/web/state.rs` (one `Broadcaster<LiveEvent>` per tenant), `src/web/ui.rs` + `workspace.rs` (queue and idle-foot handlers), `src/jobs/` (send on queue change and idle-line change), templates `_queue.html`, `_idle_foot.html`, `layout.html`.

**Interfaces — Produces:** `enum LiveEvent { Queue, Idle }`; `GET /ui/live` (authenticated, `text/event-stream`, every event carries a `data:` line, never buffered); templates use `hx-ext="sse" sse-connect="/ui/live"` with `hx-trigger="sse:queue"` / `sse:idle`. No `every Ns` trigger remains anywhere.

- [ ] **Step 1:** Write `web::live::tests::{a_queue_change_reaches_a_subscriber_as_an_event_with_a_data_line, one_tenant_never_sees_anothers_events}` and extend `tests/csp_static.rs` with `no_template_polls` (no `every \d+s`).
- [ ] **Step 2:** Run — FAIL. **Step 3:** Implement. **Step 4:** Run those and `cargo test --test browser_ask` — PASS.
- [ ] **Step 5:** Commit `feat(ui): queue and idle line arrive over SSE`.

### Task 13: Preferences: theme and density per user

**Files:** Create: `src/web/prefs.rs` (port `KIT/src/prefs.rs`). Modify: `src/store/control_schema.sql` + `control.rs` (ADDITIVE `users.theme TEXT NOT NULL DEFAULT 'system'`, `users.density TEXT NOT NULL DEFAULT 'default'`), `layout.html`, every page template struct (via a shared `PageCtx`).

**Interfaces — Produces:** `enum Theme { System, Light, Dark }`, `enum Density { Default, Compact }`, `struct Prefs { theme, density }`; `Control::prefs(subject) -> Result<Prefs>`, `Control::set_prefs(subject, Prefs) -> Result<()>`; `POST /_oc/prefs` (fields `theme`, `density`), 204; `struct PageCtx { prefs: Prefs, signed_in: bool, nav: NavState }` embedded in every page template so `layout.html` renders `data-theme` / `data-density` server-side and `data-prefs="server"`; signed-out pages render `data-prefs="local"`. Top bar gets two icon buttons (theme: tooltip names the next state; text size) wired by `prefs.js`. Root font size is `100%` (`87.5%` compact) — delete `html { font-size: 16px }`.

- [ ] **Step 1:** Tests `web::prefs::tests::{a_stored_dark_theme_is_rendered_on_the_html_element, compact_density_is_rendered_on_the_html_element, the_theme_tooltip_names_the_next_state, signed_out_pages_defer_to_local_storage}`; `tests/csp_static.rs::no_css_sets_a_px_root_font_size_or_text_below_12px`.
- [ ] **Step 2–4:** FAIL, implement, PASS. **Step 5:** Commit `feat(ui): per-user theme and text size`.

### Task 14: Shell and navigation, everyday profile

**Files:** Modify: `layout.html`, `assets/css/7x-layout`, `7x-phone` (most of both is replaced by the kit's `20-layout`, `50-touch`, `profile-everyday`), `assets/manifest.webmanifest`, `assets/sw.js`. Create: `src/web/templates/_topbar.html`, `_tabbar.html` (port `KIT/templates/oc/{topbar,tabbar}.html`), `src/web/templates/offline.html`.

Decisions: destinations are the current ones (read `layout.html`'s `nav.top` and `.tabbar` before changing); the tab bar shows 3–5 as Lucide icon + label below 48rem, with capture as the primary action bottom-right; from 48rem top navigation with search (`/` focuses it) and one centred column of at most 46rem; from 90rem list + detail side by side (28rem + 52rem) on the workspace (results list, artifact detail in the pane); from 150rem a third region of 22rem (the due/day rail). Breakpoints are container queries in rem on the app body. Touch targets ≥ 44 px with 8 px gaps; `env(safe-area-inset-*)` respected. The service worker caches the shell assets and `offline.html` only, never app data. `?` opens the shortcuts dialog, Esc closes the innermost layer.

- [ ] **Step 1:** Extend `tests/browser/page_width.js` (and its Rust driver) with cases at 390 px, 1280 px and 2560 px: at 390 the tab bar is visible with 3–5 labelled items and no horizontal scroll; at 1280 the main column is ≤ 46rem wide; at 2560 list and detail sit side by side and no region is wider than its cap; every tab-bar target is ≥ 44 px high.
- [ ] **Step 2–4:** FAIL, implement, PASS (`cargo test --test browser_ask` and the page-width driver; these need the browser CI job's setup — see `.github/workflows/ci.yml` job `browser`).
- [ ] **Step 5:** Commit `feat(ui): everyday shell — tab bar, capped column, list and detail`.

### Task 15: Components, page by page

One commit per group; each group's gate is `cargo test --test rendered --test csp_static --test copy` plus the page's own handler tests. Reference markup: `KIT/templates/oc/*.html`, `KIT/src/ui/components.rs`, `DEMO/templates/notes/*`. Rules: `RULES/20-components.md`, `RULES/90-anti-patterns.md`.

Applies to every group:
- Links are `<a href>` (underlined; accent in prose, text colour in data). Buttons are `<button>` boxes or icon buttons; no text-only buttons; one primary per view. `.btn-accent` on `<a>` becomes either a link or a real button in a form.
- Icons from `assets/icons/` through one helper `crate::web::icons::icon(name: &str, size: u8) -> askama::filters::Safe<String>` (port `KIT/src/ui/icons.rs` vocabulary; add engram's meanings to the vocabulary, never improvise). Delete `_icon_hide.html`, `_icon_trash.html`. Text glyphs `⚠` `✓` used as icons become the status-symbol pattern with `aria-hidden` icon + text.
- Notices: status symbol + text between hairlines; no boxes, tints or left stripes. Empty states: one line plus the button.
- Forms: labels above fields, correct `type`/`inputmode`/`autocomplete`, validation as inline notices with 422.
- Copy buttons (`copy.js`) next to ids, tokens, URLs, hashes.
- Cards with shadows, pill badges on non-status things, stat tiles, gradients are removed. The one exception is the scroll-edge `mask-image`, which paints nothing visible; its `#000` becomes `currentColor`. Only `--shadow-float` on floating layers.
- Busy: `aria-busy` + spinner after 400 ms (`busy.js`).

Groups (each `- [ ]` is implement → gate → commit):
- [ ] **15a** Sign-in and errors: `login.html`, `error.html`, `not_found.html`, `pair.html`, `app.html`. Commit `refactor(ui): sign-in, pairing and error pages on the components`.
- [ ] **15b** Workspace: `workspace.html`, `_results.html`, `_search_verdict.html`, `_box_hint.html`, `_intent_echo.html`, `_captured.html`, `_held.html`, `_context.html`, `_keyhint.html`, `_pane_idle.html`, `_idle_foot.html`, `_queue.html`, `_sleep.html`, `_evolve.html`.
- [ ] **15c** Ask: `_answer.html`, `_ask_*.html`, `_source_lines.html`, `_mark.html`, `_disagrees.html`.
- [ ] **15d** Artifact and lineage: `artifact_detail.html`, `_artifact_detail.html`, `_artifact.html`, `_lineage.html`, `_bands.html`, `_due.html`, `day.html`.
- [ ] **15e** Corpus: `corpus.html` and its tables (kit table rules: sticky header, row height, selection frame, right-aligned actions column named "Actions" for screen readers, tabular right-aligned numbers).
- [ ] **15f** Settings, insights, extension page: `settings.html`, `_token_created.html`, `insights.html`, `extension.html`. `<select>` upgraded by `menu.js` on fine pointers only. An unconfigured subsystem (push, OIDC, speech, PDF, contained) renders no entry point — add `web::settings::tests::an_unconfigured_subsystem_shows_no_entry_point`.
- [ ] **15g** Delete what is now dead in `assets/css/7x-*.css` and `assets/app.js`; run `cargo run`-free check: `tests/csp_static.rs::no_literal_colour_outside_the_token_file` (regex for `#hex`, `rgb(`, `hsl(`, `oklch(` in every CSS file but `00-tokens.css`).

### Task 16: Extension panel

**Files:** Modify: `extension/shared/panel.html`, `panel.css`, `panel.js`, `pair.js`, `extension/pack.sh`, `build.rs` (`pack`). Replace `extension/shared/inter-*.woff2` with the Atkinson files.

- [ ] **Step 1:** `tests/csp_static.rs::the_extension_panel_has_no_inline_code_or_literal_colours` (same scans over `extension/shared/`; the panel's CSS may `@import` nothing — tokens are copied into `panel.css` from `00-tokens.css` by `pack` at build time, so one source of truth).
- [ ] **Step 2–4:** FAIL, implement (tokens, type, buttons/links, notices, short copy), PASS; `cargo build` still packs both archives.
- [ ] **Step 5:** Commit `refactor(extension): panel on the manifest's tokens and components`.

### Task 17: Android

**Files:** Modify: `android/app/src/main/kotlin/…/ui/Theme.kt`, `Sizes.kt`, `Words.kt`, `RowWords.kt`, `Nav.kt`, every `*Screen.kt`, `res/values/strings.xml`, `res/font/`. Add Atkinson Hyperlegible Next TTFs (400/500/600) to `res/font/` with the OFL text under `android/app/src/main/assets/licenses/`; obtain the TTFs from the upstream named in the manifest's `scripts/vendor-assets.sh`; delete `inter_*.ttf`.

Decisions: colours are the manifest tokens for both themes (light when the system expresses no preference); type is Atkinson for interface, JetBrains Mono for ids/timestamps; no text below 12 sp; copy follows the Global Constraints (cut every explanatory string in `strings.xml` and `Words.kt`); every confirmation `AlertDialog` for a reversible action becomes act-then-`Snackbar` with Undo (6 s) calling the restore endpoints from Task 11; source delete and log wipe use a typed-name dialog; touch targets ≥ 48 dp; bottom navigation with 3–5 labelled destinations; status as symbol + colour. Unchanged by rule: judging stays behind Settings and unbadged; offline says "Server unreachable".

- [ ] **Step 1:** Add JVM unit tests (existing test source set in `android/core` or `app`): `ThemeTest.palette_matches_the_manifest_tokens` (parse `../assets/css/00-tokens.css` and compare to `Theme.kt` constants), `WordsTest.no_string_is_longer_than_a_gloss` (≤ 12 words, no `!`, over `strings.xml`).
- [ ] **Step 2–4:** FAIL, implement, `./gradlew :app:testDebugUnitTest :app:assembleDebug` PASS (as the `android.yml` workflow runs it).
- [ ] **Step 5:** Commit `refactor(android): manifest palette, type, copy and undo`. Device pass is batched in Task 27.

---

## Stage 4 — Code

### Task 18: Lint table and unwraps

**Files:** Modify: `Cargo.toml`, `src/**/*.rs`, `build.rs`, `examples/`. Create: `rustfmt.toml`, `rust-toolchain.toml` (from `TPL/`, channel unchanged from what CI uses).

Decisions: `[lints.rust] unsafe_code = "deny"` (not `forbid`, which cannot be overridden) and `[lints.clippy] unwrap_used = "deny"`, `expect_used = "deny"`. The FFI modules behind `contained` (`src/infer/whisper.rs`, the llama binding module, the sqlite-vec registration in `src/vector/sqlite.rs`) carry `#![allow(unsafe_code)]` at module top with a one-line why; nothing else may. Tests: `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]` at crate root and in `tests/*.rs`. `build.rs` returns `Result` from `main` instead of `expect`/`panic!`. A `Mutex` poison `unwrap` becomes `unwrap_or_else(PoisonError::into_inner)`; an impossible state becomes an `Error::Internal(&'static str)` variant (add it, maps to 500), not a panic.

- [ ] **Step 1:** Add the lint tables; run `cargo clippy --all-targets -- -D warnings 2>&1 | grep -c 'unwrap_used\|expect_used\|unsafe_code'` — record the count (expect roughly 255 + 9).
- [ ] **Step 2:** Fix file by file, committing per top-level module (`store`, `core`, `jobs`, `infer`, `web`, `cli`, `backup`, `vector`, rest) with `cargo test <module>::` as the gate.
- [ ] **Step 3:** Run `cargo clippy --all-targets -- -D warnings` and `cargo clippy --all-targets --features contained -- -D warnings` — expected: clean. Add `tests/csp_static.rs::unsafe_is_allowed_only_in_the_contained_ffi_modules` (grep for `allow(unsafe_code)` and compare to the fixed list).
- [ ] **Step 4:** Commit(s) `refactor: deny unwrap, expect and unsafe outside the contained FFI`.

### Task 19: One error type, redacted logs

**Files:** Modify: `src/error.rs`, library modules using `anyhow` (grep; `src/main.rs` and `src/cli/` may keep `anyhow` at the binary edge), `src/web/mod.rs` (`TraceLayer`), `Cargo.toml` (`tower-http` feature `sensitive-headers`).

- [ ] **Step 1:** Tests `error::tests::every_variant_maps_to_a_status_and_one_sentence`, `web::tests::authorization_and_cookie_headers_are_marked_sensitive` (the request reaching the trace layer has both headers flagged `is_sensitive()`).
- [ ] **Step 2–4:** FAIL, implement (`SetSensitiveRequestHeadersLayer` for `authorization`, `cookie`, and `set-cookie` on responses; grep `tracing::` calls for token or secret fields), PASS.
- [ ] **Step 5:** Commit `refactor: one error type in the library, sensitive headers redacted`.

### Task 20: Compile-time-checked queries

**Files:** Modify: `Cargo.toml` (sqlx feature `macros`; still no `migrate`), every file with `sqlx::query` (see `grep -rc 'sqlx::query' src`), `src/cli/` (new subcommand), `.github/workflows/ci.yml`, `.gitignore`. Create: `.sqlx/` (committed), `.env.example` with `DATABASE_URL=sqlite://target/sqlx-prepare.db`.

**Interfaces — Produces:** hidden CLI subcommand `engram dev-schema <path>`: creates an empty SQLite file and runs `Control::migrate` and `Store::migrate` on it (plus the SQLite vector tables that are plain tables). The two schemas share no table name (checked 2026-11-11), so one reference database serves both; `cli::tests::dev_schema_builds_a_database_with_both_schemas` also asserts no name is defined twice, so a future clash fails loudly. Record the prepare procedure in `AGENTS.md` (Task 26):

```sh
cargo run -- dev-schema target/sqlx-prepare.db
DATABASE_URL=sqlite://target/sqlx-prepare.db cargo sqlx prepare -- --all-targets --all-features
```

Stay runtime, each with a one-line why: the migrators' own statements, `vec0` virtual-table queries in `src/vector/sqlite.rs`, the one `format!`-built statement, anything with a dynamic `IN (…)` list or dynamic `ORDER BY`.

- [ ] **Step 1:** Implement `dev-schema`; test `cli::tests::dev_schema_builds_a_database_with_both_schemas`.
- [ ] **Step 2:** Convert module by module (`src/store/*.rs` first, largest first), running `cargo sqlx prepare` and `cargo test store::<module>` after each; commit per module group.
- [ ] **Step 3:** Convert queries in tests the same way.
- [ ] **Step 4:** CI: set `SQLX_OFFLINE: "true"` in the workflow env; add a step `cargo sqlx prepare --check -- --all-targets --all-features` after building the reference database (install `sqlx-cli` with `taiki-e/install-action`, SHA-pinned).
- [ ] **Step 5:** Run `SQLX_OFFLINE=true cargo check --all-targets` and `--features contained` — expected: clean. Add `tests/csp_static.rs::every_runtime_query_says_why` (each remaining `sqlx::query(` / `query_as::<` is preceded by a `// runtime:` line).
- [ ] **Step 6:** Commit(s) `refactor(store): compile-time-checked queries`.

### Task 21: The comment pass

**Files:** every tracked text file except `vendor/`, `Cargo.lock`, `.sqlx/`, fixtures.

Rule per comment (`RULES/80-code.md`, design anti-goals):
1. Narrates what the code does, restates a signature, is a section banner, or tells history ("used to", "was", "this replaced") → delete.
2. Records a constraint the code cannot show (an RFC clause, a measured threshold, a platform quirk, a guarded past bug) → keep as one or two lines stating the constraint, shorter than the code it sits on.
3. Exists because the code is unclear → rename or extract until it is not needed, then delete it. No behaviour change; the module's tests are the gate.
4. Doc comments on public items: one line saying what it is for, only where the name does not.
5. Test doc comments: delete; the test name is the sentence. Rename tests whose names are not sentences.
6. `Cargo.toml` dependency rationale: one line per dependency at most. Workflow, CSS, JS, template (`{# #}`), Kotlin and shell comments: same rules.

- [ ] **Step 1:** Add `tests/csp_static.rs::no_comment_block_is_longer_than_three_lines` (consecutive `//`, `///`, `#`-in-toml, `{# #}`, `/* */` lines; limit 3) and `::comment_lines_are_under_8_percent_of_src` (currently ~21 %).
- [ ] **Step 2:** Run — expected: FAIL.
- [ ] **Step 3:** Work module by module in this order, committing each with `cargo test <module>::` green: `src/store`, `src/core`, `src/jobs`, `src/infer`, `src/vector`, `src/web`, `src/auth`+`src/mcp`+`src/backup`+`src/cli`+`src/eval`+root files, `tests/`+`examples/`+`build.rs`+`Cargo.toml`, `assets/`+templates, `extension/`, `android/`, `.github/`+shell scripts.
- [ ] **Step 4:** Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --test csp_static` — expected: PASS.
- [ ] **Step 5:** Final commit of the stage `refactor: comments say why, briefly, or are gone`.

---

## Stage 5 — Repo and releases

### Task 22: Licence, deny, third-party licences

**Files:** Create: `LICENSE-MIT`, `LICENSE-APACHE`, `deny.toml` (from `TPL/deny.toml`; `allow-git` adds `https://github.com/overcuriousity/docling.rs`; the RUSTSEC-2026-0459 ignore moves here with its written reachability argument — take the argument from the current CI/audit config and commit history), `about.toml`, `about.hbs`, `THIRD-PARTY-LICENSES`, `SECURITY.md` (from `TPL/`). Modify: `Cargo.toml` (`license = "MIT OR Apache-2.0"`, `repository`), `android/` and `extension/` manifests where they carry a licence field.

- [ ] **Step 1:** `cargo deny check` (install prebuilt). Expected: advisories, bans, sources green. **If licences fail: stop and report the crate and licence to the owner.**
- [ ] **Step 2:** `cargo about generate about.hbs > THIRD-PARTY-LICENSES`; append the font, Lucide, htmx and whisper.cpp/llama.cpp notices.
- [ ] **Step 3:** Commit `chore: licence, cargo-deny policy, third-party licences`.

### Task 23: CI

**Files:** Modify: `.github/workflows/ci.yml`, `android.yml`, `release.yml` (pins only here), `.github/dependabot.yml` (unchanged unless the comment pass touched it).

- [ ] **Step 1:** Replace the `audit` job with `deny` (`cargo deny check`). Add jobs/steps: `oc-check .` (`cargo install --git https://github.com/overcuriousity/overcuriousity-dev-manifest --tag v2026.11.1 oc-check --locked`), `shellcheck install.sh build-lowmem.sh extension/pack.sh scripts/*.sh`, `node --test 'assets/js/*.test.mjs'`, the sqlx check from Task 20. Keep fmt, clippy, tests, contained, msrv, qdrant integration, browser, android.
- [ ] **Step 2:** Pin every `uses:` to a full commit SHA with the tag as a trailing comment (`gh api repos/<owner>/<repo>/commits/<tag> --jq .sha`). Top-level `permissions: contents: read`; widen per job only where needed.
- [ ] **Step 3:** Run `oc-check`'s rules locally through the test suite (`cargo test --test rendered --test csp_static`); run `shellcheck` locally; validate workflow YAML with `actionlint` if available.
- [ ] **Step 4:** Commit `ci: cargo-deny, oc-check, shellcheck, SHA-pinned actions`.

### Task 24: Changelog and continuous release

**Files:** Create: `CHANGELOG.md` (Keep a Changelog; one `## [Unreleased]` section describing this branch: Changed/Removed/Security, including the clean break on tokens and sessions, the removed config keys, the dropped bookmarklet). Modify: `.github/workflows/release.yml`.

Decision: the release workflow, after computing `YYYY.MMDD.N`, rewrites `## [Unreleased]` to `## [<version>]` in the release checkout (where it already stamps `Cargo.toml`), fails if there is no `## [Unreleased]` heading, and commits nothing back to master — master always carries `## [Unreleased]`; the released tree and bundle carry the versioned heading. If the section is empty the release still proceeds with the body `No user-visible changes.` The GitHub release notes are that section.

- [ ] **Step 1:** Extract the rewrite into `scripts/stamp-changelog.sh <version>`; test it with `scripts/test-stamp-changelog.sh` (a temp changelog in, assert the heading and that a missing heading exits non-zero); shellcheck both.
- [ ] **Step 2:** Wire into `release.yml`; add the agreement check (tag = Cargo version = changelog heading) as a step after stamping.
- [ ] **Step 3:** Commit `ci(release): changelog stamped at release, versions checked for agreement`.

### Task 25: Offline bundle, installer, unit, nginx, AIRGAP

**Files:** Create: `scripts/bundle.sh` (from `TPL/scripts/bundle.sh`, `app` → `engram`), `deploy/engram.service` (from `TPL/deploy/app.service`), `deploy/nginx.conf` (from `TPL/`, SSE routes `/ui/live` and `/ui/ask/` unbuffered), `AIRGAP.md` (from `TPL/AIRGAP.md`, adapted: install, TLS with a local CA or self-signed, plain http with `allow_plain_http`, offline build from the vendored archive). Delete: root `engram.service`. Modify: `install.sh`, `release.yml`, `Cargo.toml` (`sd-notify = "0.4"`), `src/main.rs`, `README.md`.

**Interfaces — Produces:**
- `install.sh --archive <bundle.tar.gz>`: no network, verifies `SHA256SUMS`, installs to `~/.local/bin` by default, `--system` (root only) to `/usr/local/bin` plus unit and config example, never calls sudo. The online path stays the default with no flag.
- engram sends `READY=1` after the listener is bound and `WATCHDOG=1` at half the unit's `WatchdogSec` (no-ops when `NOTIFY_SOCKET` is unset).
- Release: per target a bundle with binary, `install.sh`, `deploy/engram.service`, `deploy/nginx.conf`, `config.example.toml`, licences, `THIRD-PARTY-LICENSES`, `CHANGELOG.md`, `SHA256SUMS`; plus a vendored source archive (`cargo vendor`, including the git-patched docling crates); `actions/attest-build-provenance` (SHA-pinned; job permissions `id-token: write`, `attestations: write`). The APK stays.
- aarch64: try `aarch64-unknown-linux-musl` with `cargo-zigbuild` as the template does. If the bundled SQLite or another C dependency does not build static, keep gnu and add the deviation row in Task 26 with the failing crate named.
- The hardened unit's `MemoryDenyWriteExecute` and `ProtectSystem=strict` must hold for the default build; state in `AIRGAP.md` in one line if the `contained` build needs `MemoryDenyWriteExecute=no`.

- [ ] **Step 1:** `scripts/test-install.sh` (port `<dir>/scripts/test-install.sh`): build a bundle from a debug binary, install with `--archive` into a temp prefix with networking unavailable (`unshare -n` where permitted, else `PATH` without curl), assert the binary runs `--version`; corrupt one byte and assert the install refuses.
- [ ] **Step 2:** Test `main` notify: `tests/notify.rs::ready_is_sent_once_listening` (bind a `UnixDatagram`, set `NOTIFY_SOCKET`, start the server on port 0, read `READY=1`).
- [ ] **Step 3:** Implement; run both tests, `shellcheck`, `systemd-analyze verify deploy/engram.service` if available.
- [ ] **Step 4:** Commit `feat(ops): offline bundle and installer, hardened unit, nginx reference, AIRGAP`.

### Task 26: AGENTS.md and ROADMAP

**Files:** Create: `AGENTS.md` (from `TPL/AGENTS.md`). Modify: `ROADMAP.md`, `README.md` (Docs section links AGENTS, CHANGELOG, SECURITY, AIRGAP).

`AGENTS.md` content: one line on what engram is; manifest version **v2026.11.1** with the link; profile **everyday**; layout of `src/`; commands (`cargo test`, both clippy invocations, `cargo deny check`, `oc-check .`, `node --test`, the sqlx prepare procedure); and the deviations table from the design, final state, one line of reason each — Qdrant; `unsafe` in the contained FFI modules (list them); engram's migrators instead of sqlx migrations; no OpenAPI; no passkeys; the vector background; config keys under `[server]` and top-level sections, marked **to be streamlined later**; bearer-without-cookie origin exemption (only if Task 7 Step 0 kept it); continuous release; aarch64 gnu (only if Task 25 kept it); runtime queries listed by reason.

`ROADMAP.md`: add one entry `Config shape: move to the manifest's layout (top-level deployment keys, settings under [app])`.

- [ ] **Step 1:** Write both. **Step 2:** Commit `docs: AGENTS.md with the manifest version and deviations`.

### Task 27: Verify, device pass, PR

- [ ] **Step 1:** Walk `RULES/99-review-checklist.md` item by item against the running app (`cargo run` with a scratch config and data dir; never the live base). Record each item's result.
- [ ] **Step 2:** Start the full suite once in the background: `cargo test --all-targets` and `cargo test --all-targets --features contained`; meanwhile run `cargo fmt --check`, both clippy invocations, `cargo deny check`, `node --test 'assets/js/*.test.mjs'`, the browser tests at 390 / 1280 / 2560 px, `./gradlew :app:testDebugUnitTest :app:assembleDebug`, `scripts/test-install.sh`. Fix what fails; report the full-suite result when it lands.
- [ ] **Step 3:** Upgrade rehearsal: build the binary from `master` in a separate worktree, boot it once against a scratch data dir with local auth, capture a note and mint a token, stop it; then boot this branch's binary against the same data dir. Confirm the token-removal warning appears once and a second boot is silent, sign in, search for the note, open Trash.
- [ ] **Step 4:** Device pass (owner's hardware, after the PR is open): write the checklist into the PR description — pair, capture, search, ask, delete + Undo, reminders, both themes — and mark it as not yet run.
- [ ] **Step 5:** Push the branch and open one PR. Body: what changed per stage, the deviations table, the breaking changes (tokens, sessions, removed config keys, bookmarklet), what was verified and what was not, ending with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.

---

## Notes for whoever executes this

- Tasks 15, 18, 20 and 21 are each many commits; treat each listed group as its own reviewable unit.
- Tasks 18–21 touch almost every Rust file. Do them in the listed order (lints, errors, queries, comments) so each file is rewritten for meaning before its comments are judged.
- The production deployment reads the tenant DB and `artifacts_<tenant>_v2`, not the paths the repo-local config names. Nothing in this plan touches production; the deploy itself is the owner's step after merge, and needs `server.public_url` set first.
