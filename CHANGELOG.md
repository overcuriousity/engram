# Changelog

All notable changes are listed here, in the format of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions are CalVer (`YYYY.MMDD.N`); the release workflow turns `[Unreleased]` into the version it publishes.

## [Unreleased]

### Added
- `scripts/probe-decide.sh` measures a decision endpoint.
- Insights shows what the base is unsure about, with a button per answer. Nothing produces these rows yet.
- Decide: a typed question answered from the notes that bear on it, with each note's own reading. Judging a decision confirms the notes that agree and sends the ones against to the pair judge. `POST /api/v1/decide`, `/decide/stream`, `/decisions/{id}/verdict`; thresholds under `[decide]`. In the web UI beside Search and Ask (Alt+Enter). Also the MCP tool decide and engram -d. Not on the Android app, which runs no decision model.

### Changed
- **`[infer.decide]` is required.** engram needs a decision model (System One, `POST {base_url}/systemone`); a config without the block is refused at start. Copy it from `config.example.toml`.
- engram follows the overcuriousity dev manifest v2026.11.1, profile everyday; deviations are listed in `AGENTS.md`.
- Pages, the extension panel and the READMEs say less: labels and short glosses only.
- The interface uses the manifest's tokens, Atkinson Hyperlegible Next and Lucide icons; theme and text size are stored per user.
- The top bar and phone tab bar are the kit's; prose pages are one centred column of at most 46rem.
- The queue, the idle line and the due band update over server-sent events instead of polling.
- Deleting an artifact moves it to Trash, with Undo; Trash is emptied after 30 days. `DELETE /api/v1/artifacts/{id}` trashes, `POST /api/v1/artifacts/{id}/restore` restores.
- Revoking a token acts at once, with Undo for 30 days.
- Purging recorded searches and deleting a source need their name typed.
- The extension panel takes its palette and fonts from the server's own token file.
- Duplicates, links and burials are judged by the decision model, not a chat model. A judgement it is sure of is carried out, one it is torn on is asked on Insights under "Unsure", the rest are left. Merges and rescues are still written by the chat model, each as its own unit after a decision asked for it.
- A link shows its kind ("An example of this", "Another way to do this") instead of a written sentence. Burials are no longer explained by a reason.
- Defaults raised: `consolidate.max_dedupe_per_tick` 20, `associate.judge_per_sweep` 30, `reap.max_judged_per_run` 50.
- Every note is read once by the decision model: its kind, whether it records a decision, whether it will go out of date, whether it stands alone. Verbatim passages get a category this way. A category you set is never overwritten.
- The chat model no longer categorises, pins or links at capture; it writes. Reminders are still read by it, when two quick questions say there may be one.
- Background rewrites (merge, rescue, condense, generate, capture synthesis) are checked against their originals and refused when they drop or add something.
- A pursuit is written only when its sources hold the answer; a used passage that already stands alone is not rewritten.

### Removed
- `auth.oidc.redirect_url`: derived from `server.public_url`. A config that still sets it is refused.
- `auth.oidc.allowed_emails`: admission is by subject or group. A config that still sets it is refused.
- `--i-know-this-is-insecure`: local auth binds to loopback only; put a proxy in front.
- The bookmarklet. iOS uses the Shortcut.
- Native `confirm()` dialogs, inline scripts, inline styles and handlers.

### Security
- **All API tokens from before this version stop working.** Tokens are now `engram_<id>.<secret>`; the upgrade deletes the old ones and logs how many. Mint new tokens in Settings and pair the extension, the phone and MCP clients again.
- **Everyone is signed out once.** Sessions move to tower-sessions in the control database; the cookie is `__Host-engram-session` (HttpOnly, Secure, SameSite=Lax), or `engram-session` over allowed plain http. Deleting a user signs every browser out.
- New `[server]` keys: `public_url` (required unless `bind` is loopback), `trusted_proxies`, `allow_plain_http`. Secrets may come from `ENGRAM__…_FILE` or a `file://` value.
- Every HTML response carries a strict CSP, `nosniff`, a referrer policy, COOP and, over https, HSTS; a foreign `Host` gets 421.
- Cookie-authenticated writes must come from this origin. Sign-in and pairing are limited to 10 a minute per client, `/api/v1` and `/mcp` to 600.
- OIDC users are bound to the issuer that first signed them in. With OIDC, `/mcp` accepts IdP access tokens and serves `/.well-known/oauth-protected-resource`.
- `Authorization`, `Cookie` and `Set-Cookie` are redacted from traces.
- `cargo-deny` replaces `cargo-audit`; the advisory ignores and their reasons are in `deny.toml`.
