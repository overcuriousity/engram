# Pairing in one scan (Part C of the Android companion)

Written 2026-09-16, against the tree as it stands on `feat/web-push` with
Part A (Web Push to the specification) already on it. This is the second of
the three server-side parts in `2026-09-08-android-companion-design.md`, and
the last one the first installable app needs.

## What it is for

A page behind a session renders a QR code. The app scans it and is paired.
Nothing else is visible to the person doing it.

The extension already pairs, through `/ui/pair` and `launchWebAuthFlow`, by
minting a long-lived bearer token and handing its plaintext to the redirect
sink. That is right for a value a browser carries straight into the extension
that asked, and wrong for a value drawn as a picture on a screen in a room: a
photograph of the screen would be the credential. So the code in the picture is
a **grant**, not a token — single-use, alive for two minutes — and the app
trades it for the real token over TLS in a POST. The long-lived credential is
never rendered.

## The URI

```
engram://pair?o=<origin>&c=<code>&v=<server version>[&f=<fingerprint>]
```

- `o` — the origin, exactly what `pair::request_origin` computes from the
  request: `https` unless the proxy said otherwise or the host is loopback.
  Percent-encoded with the local `urlencode` in `pair.rs`.
- `c` — the grant code: 32 random bytes, base64url without padding.
- `v` — `CARGO_PKG_VERSION`, so the app can say *this server is older than I
  expect* rather than fail strangely later.
- `f` — present only when the operator set `[server] tls_fingerprint`; the
  SHA-256 of the certificate's SubjectPublicKeyInfo, base64url without
  padding, 43 characters. Absent, the app pins on first use.

The query is built with the same `urlencode` the extension redirect uses, and
the page shows the URI as text under the picture, so a scanner that hands over
a string rather than opening a scheme still gets the whole of it.

## The page

`GET /ui/app` — "Pair the app", in the Settings section of the layout. It
renders a paragraph saying which origin the phone will be pointed at, and one
button. It mints nothing: the extension page has the same rule for the phone
token, and for the same reason — a credential that appears whenever a page is
opened is a credential nobody remembers asking for.

`POST /ui/app/grant` — mints a grant for the session's subject and renders the
same page with:

- the QR code, as inline SVG;
- the URI as text;
- one line: *Good for two minutes, and for one phone. Press again for another.*

Both routes take `Tenant`, so a bearer token can reach them too; that is how
the tests drive them, and it costs nothing to allow.

The extension install page's *On a phone* section gains one sentence linking
here, before the bookmarklet, because a phone with the app has no use for the
bookmarklet.

## The grant

A table in the control database, beside `api_tokens`:

```sql
CREATE TABLE IF NOT EXISTS pair_grants (
  id         TEXT PRIMARY KEY,
  code_hash  TEXT NOT NULL UNIQUE,
  subject    TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  claimed_at INTEGER
);
```

`code_hash` is the SHA-256 of the code, hex. Not argon2id, and the column
comment says why: argon2 is there to make guessing a low-entropy secret slow,
and this secret has 256 bits of entropy and is dead in two minutes. A fast hash
keeps the stored value useless to a reader of the database — which is the
property `api_tokens` keeps — and lets the claim be one indexed query rather
than a scan hashing every live row.

Life: 120 seconds, a constant in `auth::grants`. Expired rows are deleted at
every mint, the way `purge_expired_sessions` is run; there is no reaper and
none is needed, because the table only grows when someone presses the button.

## The claim

`POST /api/v1/pair/claim`, mounted in the API router beside `push::routes()`.
No bearer: the code is the credential. Body:

```json
{ "code": "…", "device": "engram for Android 1.0 · Pixel 8" }
```

One statement does the claiming:

```sql
UPDATE pair_grants SET claimed_at = ?
 WHERE code_hash = ? AND claimed_at IS NULL AND expires_at > ?
```

Rows affected is the answer. Zero — unknown, expired, or already claimed — is
`401 Unauthorized`, one status for all three so the route tells a guesser
nothing. One is a claim, and the handler mints a token through
`auth::tokens::mint` named by `device` with the request's `User-Agent`
recorded, and answers:

```
201 Created
{ "token": "engram_…", "version": "<CARGO_PKG_VERSION>" }
```

`device` trimmed and empty, or missing, is `400` through `Error::Validation`
before the grant is touched, so a malformed request does not burn a code. If
the mint fails after the claim the grant is spent and the operator presses the
button again; that is the correct direction to fail in.

The token then appears under Settings → API tokens named for the device and
carrying its user agent, and is revoked there. Nothing in Settings changes.

## The fingerprint in config

```toml
[server]
# tls_fingerprint = "…"   # SPKI SHA-256, base64url, from the certificate the
                          # phone will be shown; the QR carries it as `f=`
```

`ServerConfig.tls_fingerprint: Option<String>`. Validated when the config is
loaded: base64url with or without padding, decoding to exactly 32 bytes, and
stored normalised without padding. A value that fails is a config error naming
the key, so a typo is found at start-up and not by a phone that refuses every
connection. It is a config key and not an `instance` row because the
certificate belongs to whatever terminates TLS in front of the process, which
the process cannot see; it has to be told.

## The QR

Rendered on the server by the `qrcode` crate (0.14.1, default features off:
the SVG renderer is in the crate itself, and the `image` feature is what pulls
in the weight). Inline SVG in the template, marked safe, sized by CSS to about
256 px so it scans from a laptop screen at arm's length. Error-correction level
M. No JavaScript, no image route, nothing to cache.

## Files

New:

- `src/store/grants.rs` — `insert_grant`, `claim_grant`, `purge_expired_grants`
  on `Control`.
- `src/auth/grants.rs` — `mint` (code + row, returns the plaintext code) and
  `claim` (code + device + user agent → the token plaintext), plus `TTL`.
- `src/web/app.rs` — the page, the grant press, the URI builder, the claim
  route, `routes()` for the API side and `app_router()` for the UI side.
- `src/web/templates/app.html`.

Touched:

- `src/store/control_schema.sql` — the table.
- `src/store/mod.rs`, `src/auth/mod.rs`, `src/web/mod.rs` — module lines and
  router merges.
- `src/web/api.rs` — `.merge(crate::web::app::routes())`.
- `src/config.rs` — the key and its validation.
- `config.example.toml` — the commented line.
- `src/web/templates/extension.html` — the link.
- `Cargo.toml` — `qrcode`, with the comment saying why that crate.

## Tests

Store, in `grants.rs`:

- a grant claims once, and a second claim of the same code returns `false`;
- a row past its expiry cannot be claimed;
- purge removes expired rows and leaves live ones.

Auth, in `auth/grants.rs`:

- the code is at least 40 characters and does not start with `engram_`, so
  it can never be mistaken for, or verify as, a token;
- the stored hash is not the code.

Web, in `app.rs`, through `test_support`:

- `/ui/app` and `/ui/app/grant` are not served unauthenticated;
- GET renders no `engram://` URI;
- POST renders an `engram://pair?o=https%3A%2F%2Fengram.test&c=` URI and an
  `<svg`;
- the code claimed with a device name answers 201, the token opens
  `POST /api/v1/capture`, and the token list shows a row with that name;
- claiming the same code again is 401;
- a grant whose `expires_at` is set into the past directly in the store is 401;
- a claim with an empty device is 400 and the grant is still claimable;
- `f=` is absent by default and present, with the configured value, when
  `tls_fingerprint` is set;
- a `tls_fingerprint` that does not decode to 32 bytes fails config load.

## Out of scope

The app. Rate limiting on the claim route: the code space is 2^256 and the
window two minutes. Any change to how tokens are hashed or listed. Carrying
the fingerprint anywhere but the QR.
