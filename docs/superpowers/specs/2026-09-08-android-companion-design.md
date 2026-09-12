# The Android companion, and the watch behind it

Written 2026-09-08. A programme rather than a change: seven parts, three of
them on the server, four on the phone. Each part is worth shipping alone, and
the order they ship in is not the order they are listed in — see *Order of
work*.

Deliberately short on detail. The parts furthest out will be specified again
against the tree as it is when they start, and detail written today about code
that has not been touched yet is detail written to be wrong. What this document
fixes is the shape: which layer owns what, which seams the later parts hang
from, and what the phone is allowed to assume about the server.

## Why

engram already says it has three doors. On a phone, one of them stops at the
door frame.

**The share sheet never appears.** `assets/manifest.webmanifest` declares a
`share_target` and `src/web/share.rs` is waiting behind it, and for a Chromium
user that works. Web Share Target is a Chromium-only specification; Firefox for
Android does not implement it. Install engram from Firefox and you get the
icon, the offline shell and no entry in the share sheet at all. A native
`<intent-filter>` comes from the system rather than from the browser, and this
is the single largest thing the app buys.

**Reachability is the normal case, not the edge case.** A self-hosted engram
lives behind a VPN, on a home LAN, or behind a certificate the phone's trust
store has never heard of. A browser turns each of those into a fight, and a
share that fails because the server is unreachable is a capture that never
happens — which is the one failure this project cannot tolerate, because
capture is the gesture the whole system is built around.

**Reminders push outward.** `src/jobs/remind.rs` POSTs to a URL the operator
saved. The server has to be able to reach the phone, which is exactly the
direction that does not work for a home deployment. UnifiedPush is how that is
solved, and engram's implementation of it is a version behind.

**And a watch is coming.** A Pebble Time 2 app will sit on top of whatever the
phone app becomes. It is not built here, but the layer it needs is, and getting
that layer wrong is the expensive mistake this document exists to prevent.

## The seven decisions this rests on

**1. The app is a client, never a second engram.** Nothing is embedded, ranked
or synthesized on the device in this programme. Running engram itself on the
phone is a separate programme with a separate prerequisite (the ROADMAP item
*A first run that costs one process*), and it is out of scope here. The layer
in Part D is drawn so that programme is a change of base URL rather than a
rewrite, and that is the only concession made to it.

**2. What the app needs, the API gets — not the app.** No endpoint is added
for a client. `/api/v1` becomes as wide as the web UI because the README
promises three equal doors and today one of them is lower than the other. The
CLI and the MCP server get the same widening for free.

**3. UnifiedPush, to the current specification, and nothing beside it.** No
local-alarm fallback, no polling. This is a decision taken deliberately with
its cost known: a reminder only rings when the push service is reachable from
the server. Paying the specification debt is worth it on its own.

**4. One layer, five entrances.** The screens, the share intents, the push
receiver, the quick-capture surfaces and — later — the watch receiver are five
thin entrances onto one core. No screen holds knowledge. A `PebbleDataReceiver`
is a `BroadcastReceiver` and lives outside every Activity, so any design where
the Activity knows something is a design the watch cannot use.

**5. Everything read is a cache with a provenance; everything written is a
queue.** Read models carry the origin they came from and when they were
fetched, and a stale screen says so instead of lying. The outbox is the only
authoritative state on the device, and it holds every write — a capture, a
reminder marked done, a duplicate resolved — not just captures.

**6. A capture is accepted before it is delivered.** The outbox takes it,
copies its files into app storage, and answers the user immediately. Delivery
is a background concern. A share that arrives while the VPN is down is stored,
not lost.

**7. Trust is established once, at pairing, and pinned after.** One scan
carries the origin, a short-lived grant and the version. The certificate is
pinned from what the app sees during that scan.

## What this relies on, and what is already there

More is built than a first look suggests.

- **Device credentials.** `auth::tokens::mint` already mints a named,
  per-device bearer token, hashes it with argon2id, records the user agent that
  asked, and shows the plaintext exactly once
  (`src/web/extension.rs`, `src/store/auth.rs`, `api_tokens` in
  `control_schema.sql`). The app needs a different *delivery*, not a different
  credential.
- **Origin learning.** `pair::request_origin` derives the address a deployment
  is reached at from the request, and defaults to `https` unless the host is
  loopback, precisely because a bearer token must not be handed out over
  cleartext. The QR carries what that function already computes.
- **Capture is one door for an unclassified blob.** `POST /api/v1/capture` and
  `read_capture_parts` take text, a URL, one file or four, and `code_for`
  answers `200` for something already held, `202` while it is still being
  read, `201` for a complete capture. The "held for review" state a share can
  land in is therefore already expressible in JSON — the phone does not need
  the corpus page to learn it.
- **Loose matches are already honest over the API.** `SearchResult.weak`
  (`src/core/search.rs:179`) is what `_results.html` renders its
  *Relevance falls off here* rule from. The divider is presentation; the fact
  crosses the API already.
- **The reminder ladder.** `LEADS` in `src/jobs/remind.rs:18` is 48h, 12h, 3h,
  30m and the moment itself, one message per wake however many are owed, with
  `notified_at` marking rungs already sent. None of that changes.

And what is not there: `/api/v1` covers capture, corpora, search, ask,
resurface, consolidation, status and moments. Insights, gaps, pair review,
lineage, the day page and personal settings exist only as server-rendered HTML
across roughly sixty routes and forty-five templates. That gap is Part B, and
it is the largest single piece of work in this programme.

---

## Part A — Web Push, to the specification

### The defect

`src/jobs/remind.rs:141` sends a UnifiedPush message as a bare body:

```rust
Target::UnifiedPush { endpoint } => {
    http.post(endpoint).body(format!("{title}\n{message}")).send().await
}
```

That is UnifiedPush 1.x behaviour. The current specification puts plain Web
Push on the application-server side: RFC 8030 for delivery, RFC 8291 for
`aes128gcm` payload encryption, RFC 8292 for VAPID. A connector built on the
current Android library will receive this plaintext and fail to decrypt it. The
app cannot be written against the server as it stands.

Two things follow that are worth more than the compliance.

**The push service stops reading your reminders.** Today the text of what you
promised to do travels in the clear through whichever push service the endpoint
belongs to. Under RFC 8291 it is ciphertext the service cannot open. For a
project whose pitch is that nothing leaves your machine, the current behaviour
is a quiet exception to that claim.

**The payload can carry identifiers.** `compose()` at `remind.rs:173` produces
a title and up to `BODY_LINES` of prose, with no ids anywhere — so a
notification action has nothing to call. Once the payload is opaque to
everything between the two ends, it can be JSON, and *done* and *snooze* can sit
in the notification itself.

### The change

`Target::UnifiedPush` grows from an endpoint to `{ endpoint, p256dh, auth }`,
which is what a UnifiedPush registration hands back. `notify_targets` reads the
three; the `notify` JSON in `control_schema.sql` gains the two keys.

The instance holds one VAPID keypair, generated on first boot. The control
database has no home for a fact like this — it holds users, sessions, tokens
and the queue and nothing else — so this is the first row of a one-row
`instance` table, and later instance-wide facts land beside it. It is
instance-wide rather than per-user because it identifies the sender to the push
service, and the sender is the deployment.

Encryption follows RFC 8291 as written: an ephemeral P-256 key agreed against
`p256dh`, HKDF salted with `auth`, one record, `Content-Encoding: aes128gcm`,
and an `rs` greater than the plaintext plus the padding delimiter plus the tag.
This is not code to hand-roll. A maintained Rust crate that implements RFC 8291
and RFC 8292 against the final RFCs — not a draft — is a dependency worth
taking; the implementation picks it and states in a comment why, the way the
tree does elsewhere.

The payload becomes versioned JSON, and the shape is the ladder's, not a
notification's:

```json
{ "v": 1, "kind": "due", "at": 1757308800,
  "moments": [{ "id": "…", "title": "…", "at": 1757311200 }],
  "more": 0 }
```

`more` is what `BODY_LINES` counts today: a wake owing more than the payload
spells out says how many it left out, and the band has them all. The client
renders. The server stops composing prose for this channel.

Gotify does not change. It takes a title and a message and it always will, and
`deliver` already fans out to channels that disagree about shape and succeeds
if any one of them took it.

**Registration becomes programmatic.** Today an endpoint is typed into
Settings by a person. A connector learns its endpoint at runtime, from the
distributor, and it changes. So: `GET /api/v1/push/vapid` returns the instance
public key, which the app needs before it registers; `PUT /api/v1/push/unifiedpush`
takes `{endpoint, p256dh, auth}`; `DELETE` on the same path unregisters. The
Settings page keeps its manual field and gains a line saying which device
registered, so the two ways of arriving at the same row are both visible.

**Existing rows keep working.** A `unifiedpush` entry with an endpoint and no
keys is a pre-3.0 registration; it keeps receiving plaintext, and Settings
marks it as such. Nobody's reminders stop the day this ships.

### Testing

RFC 8291 publishes test vectors. The encryption is tested against them
directly — a known ephemeral key, a known receiver keypair, a known ciphertext.
Everything in `remind.rs`'s existing suite stays green unchanged: the redirect
refusal, the one-channel-refusing case, the collapsed wake, the ladder
arithmetic. Added: a keyless legacy target still gets plaintext, and a payload
that would exceed one record is refused rather than truncated.

---

## Part B — The API reaches where the interface reaches

The web UI keeps its server-rendered HTML and its htmx exactly as it is.
Nothing is rewritten to consume the API. What happens is that the facts behind
each surface get a JSON route beside the HTML one, sharing the same core call —
which is how `share.rs` and `POST /api/v1/capture` already relate.

**What crosses.** Artifact detail with its versions and lineage. Corpus read,
delete, reprocess and near-duplicate resolution. The day page. Reminders in
full, because the watch needs every one of them. Pair review with its undo.
Gaps. Insights, read-only. The sleep journal. The user's own preferences —
language, push channels.

**What does not.** API token management, by your instruction and by sense: a
credential that can mint its successors is a credential whose revocation means
less than it says. The browser extension offer, which is meaningless on a
phone. Instance configuration and `--grant-judge`, which are an operator's work
at a keyboard. And applying a tuning recommendation, which writes
`config.toml`: Insights over the API says what the sweep recommends and the
press that adopts it stays on the web, where the person who has `can_judge`
already is.

**Three rules for the shapes.** Errors map through `src/error.rs` as the
existing routes do, so a client learns one vocabulary. Anything the UI pages,
the API pages. And every read route answers `ETag` and honours
`If-None-Match` — a phone on a bad link revalidating a cache is the normal
case here, and a 304 is the difference between a usable app on a train and a
spinner.

This part is large enough to want its own document when it starts, and it can
be sliced by consumer: Part E only needs the routes the screens it draws
actually read. It does not have to land whole to be useful.

---

## Part C — Pairing in one scan

A page behind a session renders a QR code. The app scans it and is paired. That
is the whole of the user-visible part.

**What the code carries.** A URI in engram's own scheme, so a general-purpose
scanner can hand it over:

```
engram://pair?o=<origin>&c=<grant>&v=<server version>
```

`o` is what `pair::request_origin` computed. `v` lets the app say *this server
is older than this app expects* instead of failing strangely later.

**`c` is a grant, not a token.** This is the one place this design departs from
what the extension does. `extension.rs` mints a long-lived bearer token and
renders its plaintext once; that is right for a value you copy deliberately and
wrong for one displayed as a picture on a screen in a room. So the QR carries a
single-use code with a short life — two minutes is the intent — which the app
POSTs back over TLS to claim the real token. A photographed screen is worthless
by the time anyone acts on it, and the long-lived credential is never rendered
at all. A small `pair_grants` table holds the code hash, the subject, its expiry
and whether it has been claimed.

The app names itself when it claims, so the token list stays readable — which is
exactly what the `user_agent` column in `control_schema.sql:58` was added for.

**The certificate.** A deployment usually terminates TLS in nginx, so the axum
process cannot know what certificate the phone will be shown, and the QR
therefore cannot carry a fingerprint honestly. The answer is trust on first
use, narrowed: during the claim — inside the two-minute window, with the
operator standing at the screen — the app records the SPKI it was served and
pins it from then on. A publicly trusted certificate needs no pin and the app
says so. An operator who wants better than TOFU can name the fingerprint in
config and the QR will carry it; that path exists and is not the default,
because a default nobody can satisfy is a default that gets disabled.

A pin mismatch afterwards is loud and refuses. That case is the entire reason
the pin exists, and treating it as a recoverable warning would waste it.

---

## Part D — The app: foundation, capture, notification

The first installable thing. It depends on A and C, and on none of B.

**The frame.** Kotlin and Jetpack Compose, one Activity, `minSdk` 29.
Distribution through F-Droid and GitHub releases; not the Play Store, which
would demand a privacy policy and a review process for software that talks to
nothing but a server its user owns. The source lives in this repository under
`android/`, built separately — the extension is embedded in the binary by
`build.rs` so that a deployment always serves the build that matches it, and
that trick does not survive an artifact of this size. The version handshake in
Part C is what replaces the guarantee.

**The core, and the five entrances.** One module every entrance sits on, and no
Android UI type appears anywhere in it:

- `Connection` — origin, token, pin, server version. In the Keystore.
- `Transport` — the HTTP client, carrying the bearer and the pin, with the
  retry policy in one place.
- `Outbox` — every write the device owes the server, in Room. Authoritative.
- `Cache` — read models in Room, each row carrying its origin and its
  `fetched_at`.
- `Sync` — the background workers that drain the outbox and refresh the cache.
- `Push` — registration with the distributor, and decoding what arrives.

The screens read the cache and write the outbox. So does the push receiver. So
will the watch. Nothing else is allowed to talk to `Transport`.

**The outbox is the load-bearing idea.** A share arrives, its files are copied
out of the sending app's content URI into app storage — a URI granted to an
Activity is not readable an hour later, and an hour later is exactly when the
VPN comes back — and a row is written before any network call. The user is
answered immediately. A worker delivers with backoff and records what the
capture became, including `202` still-being-read and `200` already-held, so the
queue can say *stored, held for review* the way the corpus page does.

Reminder actions go through the same queue. Marking a reminder done on a train
is a write the device owes the server, and it is not a different kind of thing
from a capture; one queue, one retry policy, one place where "not yet
delivered" is visible.

**The doors.** `ACTION_SEND` and `ACTION_SEND_MULTIPLE` for text, images, PDFs
and whatever else; `ACTION_PROCESS_TEXT`, so a selection anywhere in the system
offers *save to engram* without leaving the app you are in; a quick-settings
tile and a launcher shortcut for the empty composer; camera and microphone
straight into a capture. The `engram://` scheme from Part C. A home-screen
widget is worth having and belongs in Part E, not here.

**Notifications.** The UnifiedPush connector registers, hands its endpoint to
the server, and decodes the Part A payload into a notification carrying *done*
and *snooze*. Unknown payload versions render as a plain "something is due" and
say the app is behind — a notification that fails to parse must still ring.

**The failures worth designing for.** Server unreachable: the queue holds, the
UI says so plainly, nothing is lost. Token revoked: a 401 means paired but
refused, and the app offers a rescan rather than pretending to be offline. Pin
mismatch: refuse, loudly. Distributor absent: the app explains what UnifiedPush
is and links the distributors, because a user who has never installed one will
otherwise conclude reminders are broken.

**The look.** The ask is *as good as the web interface, if not better*, and the
web interface has a handwriting: the palette in `assets/css/`, the wordmark,
the type. Compose gets a theme derived from those rather than from Material's
defaults, and this is a real workstream across D, E and F rather than something
that happens on its own. `vbg.rs` — the background drawn from the base's own
vectors — is the most distinctive thing on the web and the most expensive to
port; it is worth doing and it is not worth doing in Part D.

---

## Part E — The app: reading

Needs the read half of Part B.

Search, ask, the day page, the corpus list and its detail, artifact detail with
lineage and versions, resurface. Offline, each of these serves what the cache
holds and says when it was fetched.

Two of them are harder than they look and should not be estimated as screens.

**Search has to keep its honesty.** The rule that says *relevance falls off
here*, and the demoted rows below it that keep their rank and stop pretending,
are the reason to prefer engram over a box that always looks confident. The
fact crosses the API already as `weak`; what must not happen is a rewrite that
quietly drops the divider because it looked like chrome.

**Ask streams.** `/ask/stream` is a POST that answers as a stream, and the
badge on an unsupported command or path — the part the README calls the best
part — arrives with the answer rather than before it. The screen has to render
partial text and then re-render it annotated, and that is a different shape
from every other screen in the app.

---

## Part F — The app: judging

Needs the rest of Part B.

Duplicate pairs with their undo, gaps, insights, the sleep journal. These are
the surfaces where a person decides rather than reads, and they are the place
the app can plausibly beat the web instead of matching it: a pair review is a
queue of one decision at a time with three buttons, which is a worse fit for a
desk than for a phone in a queue at a shop. Insights stays read-only, per
Part B.

---

## Part G — The watch

Needs Part D and nothing else, which is the point of Part D's shape.

PebbleKit Android is a library in the companion app. The watch sends AppMessage
dictionaries; the phone receives them in a `PebbleDataReceiver`, which is a
`BroadcastReceiver` and therefore lives outside every Activity and every
lifecycle. It gets its own module depending on the core, and it references no UI
type. If that is true, the watch is a small piece of work; if it is not, no
amount of effort in Part G will make it one.

**What the watch is for, in order of worth.** A reminder on the wrist with
*done* and *snooze*, which the outbox already knows how to deliver. Voice
capture: the watch records, the phone forwards the audio to
`POST /api/v1/capture`, and `[infer.transcribe]` reads it — the same path the
microphone on the search box already uses. A glance at what is due today.

Search on a screen that size is a demo rather than a feature, and this document
says so now so that nobody spends a week discovering it.

**The protocol.** AppMessage dictionaries are small, so what crosses is
identifiers and short strings — never artifact bodies. The key table is fixed,
versioned, and written in one file that the C side and the Kotlin side are both
read against, because two copies of a key table drift the first time anyone is
in a hurry.

---

## Testing

Parts A, B and C are tested the way the tree tests everything else, through
`src/web/test_support.rs` and the existing patterns in `api.rs`. Specifically:
the RFC 8291 vectors in A; a grant that expires, a grant claimed twice and a
token that carries the app's name in C; and in B, every route tested for the
same thing its HTML sibling is tested for, because two doors onto one fact that
disagree are worse than one door.

On the device: the outbox is the thing that must not be trusted to review. It
gets instrumentation tests for accepting while offline, delivering on
reconnect, and surviving process death mid-queue. Payload decoding and the
Pebble key table get unit tests.

One rule holds across all of it: **no test on the phone asserts anything about
ranking.** Ranking belongs to the server, the harness that guards it lives
there, and a second set of expectations about result order maintained in Kotlin
is a second definition of correctness that will disagree with the first.

## Order of work

Not the order the parts are lettered in.

**A, then C, then D.** That is the first installable app, and it needs nothing
from B: capture, the share sheet, the queue, and reminders that ring — the
three things that motivated the project.

**Then B**, sliced by what Part E's screens read, rather than attempted whole.

**Then E, then F.** F last of the app work because judging is the least urgent
thing to do on a phone and the most pleasant to get right slowly.

**Then G**, at whatever point the Pebble Time 2 and its SDK are real enough to
build against. Nothing before G is waiting on it.

## Out of scope

Running engram itself on the device. It is a separate programme, its
prerequisite is the ROADMAP item *A first run that costs one process*, and the
only thing this document does for it is refuse to put knowledge anywhere the
change of a base URL would strand.

API token management and the browser-extension offer, by instruction. Instance
configuration, `--grant-judge`, and applying tuning recommendations, which stay
where the operator is. iOS. The Play Store. And a local-alarm fallback for
reminders, which was considered and refused in favour of UnifiedPush alone.
