# The app: foundation, capture, notification (Part D of the Android companion)

Written 2026-09-16, against the tree as it stands on `feat/web-push` with
Part A (Web Push to the specification) and Part C (pairing in one scan)
already on it. This is the first installable thing the programme document
`2026-09-08-android-companion-design.md` describes, and it needs nothing from
Part B.

## What it is for

Three things motivated the programme, and this part delivers all three:
capture from anywhere on the phone, a queue that keeps what it was given until
the server has it, and reminders that ring. Reading and judging come later, on
the same branch, and the whole app is reviewed as one pull request.

## The frame

Kotlin, Jetpack Compose, one Activity, `minSdk` 29, `compileSdk` 37. The
source lives under `android/` in this repository and is built by its own
Gradle wrapper, separately from `cargo`: the extension is embedded in the
binary by `build.rs` so a deployment always serves the build that matches it,
and that trick does not survive an artifact of this size. The `v=` in the
pairing URI and the `version` in the claim's answer are what replace the
guarantee.

Application id `io.github.overcuriousity.engram`. Distribution through
F-Droid and GitHub releases, so nothing in the app may depend on Google Play
Services: QR decoding is ZXing, not ML Kit, and push is UnifiedPush, never
Firebase.

Two Gradle modules:

- `android/core` — an Android library holding Connection, Transport, Outbox,
  Cache, Sync and Push. No Compose, no Activity, no `android.view` type
  appears in it. This is the module the watch (Part G) will depend on.
- `android/app` — the Activity, the screens, the share receivers, the tile,
  the shortcut, the UnifiedPush service, the notification, the theme.

The rule from the programme document is enforced by the module boundary:
`Transport` is `internal` to `core`, so the screens, the receivers and later
the watch can only reach the server through the outbox and the sync worker.

## Decisions, and why

- **OkHttp 5, no Retrofit.** Part D makes five kinds of call. OkHttp's
  `CertificatePinner` takes exactly the SPKI SHA-256 the QR carries, and its
  interceptors are where the bearer and the pin-on-first-use live. Ktor would
  bring a second coroutine engine for nothing.
- **kotlinx.serialization** for JSON: what the Kotlin toolchain ships, no
  reflection, and the payload's `kind` discriminator maps onto a sealed
  class directly.
- **Room** for the outbox and the cache; **WorkManager** to drain the outbox
  and to re-register push. Both survive process death and respect Doze, which
  is the entire reason the outbox exists.
- **No dependency injection framework.** An `Engram` application object
  builds the six core objects once and hands them out. A graph of six things
  is read in one file; Hilt would add a processor to hide it.
- **CameraX + ZXing** for the scanner. ZXing core is pure Java.
- **No Material dynamic colour.** The theme is the web's, below.

## `core`

### Connection

What the phone knows about its server. Stored in `EncryptedSharedPreferences`
backed by the Keystore, read into an immutable `Connection` value:

```kotlin
data class Connection(
    val origin: String,          // "https://engram.example"
    val token: String,           // "engram_…"
    val pin: String?,            // SPKI SHA-256, base64url, or null for a public CA
    val serverVersion: String,   // from the claim's answer
    val deviceName: String,      // what the token is called on the server
)
```

`ConnectionStore` exposes `current: StateFlow<Connection?>`, `set`, and
`clear`. `null` is unpaired. Clearing also cancels the sync work, unregisters
push locally, and empties the outbox's files — the rows stay, marked `held`,
so a re-pair to the same server can deliver them and a re-pair elsewhere can
show what was dropped.

### The pairing URI

`PairUri.parse(text): PairUri?` reads `engram://pair?o=…&c=…&v=…[&f=…]` and
nothing else: scheme and host must match, `o` must be an absolute `http` or
`https` URL with no path beyond `/`, `c` must be present and not empty, `v`
must be present, `f` when present must be 43 characters of base64url. A `http`
origin is accepted only for a loopback host, mirroring `pair::request_origin`
on the server. Anything else is `null`, and the screen says the code is not
an engram pairing code.

### Transport

`internal class Transport(connection: Connection)` owns one `OkHttpClient`:

- a bearer interceptor adds `Authorization: Bearer <token>` to every request
  except the claim, and a `User-Agent` of `engram-android/<version>
  (<Build.MODEL>)` to all of them, which Settings prints under the token and
  the push row. It identifies nothing else; see *Situation* below.
- when `connection.pin` is set, a `CertificatePinner` for the origin's host
  with `sha256/<pin>`; a mismatch surfaces as `PinMismatch(expected, served)`;
- a `Refused` exception for any `401`, so callers can tell *the server said
  no* from *the server did not answer*;
- timeouts of 15 s connect, 60 s read (a capture may wait on transcription),
  no automatic retry — retry is the outbox's job and lives in one place.

The claim is the one call made before a `Connection` exists.
`Pairing.claim(uri: PairUri, deviceName: String): Connection` builds a client
with no bearer and, when `uri.fingerprint` is null, a trust-on-first-use
listener: the SPKI SHA-256 of the leaf certificate the handshake actually
served is recorded into the returned `Connection.pin`, unless the chain
validated against the system store — a publicly trusted certificate needs no
pin, and `pin` stays null. When `uri.fingerprint` is set it is the pin from
the first byte. The claim POSTs `{ "code", "device" }` to
`<origin>/api/v1/pair/claim`, and a `401` is reported as *this code has
expired or was already used* — the server deliberately does not say which.

The five calls after that:

| Call | Method and path | Body |
|---|---|---|
| capture text | `POST /api/v1/capture` | `text/plain` |
| capture files | `POST /api/v1/capture` | `multipart/form-data`, parts named `file`, plus `title`, `note` |
| VAPID key | `GET /api/v1/push/vapid` | — |
| register push | `PUT /api/v1/push/unifiedpush` | `{ endpoint, p256dh, auth }`; the device is the `User-Agent` |
| moment done / snooze | `POST /api/v1/moments/{id}/done`, `…/snooze` | none / `{ "until": <unix seconds> }` |

Capture answers are kept whole: the status (`200` already held, `201` new,
`202` still being read) and the JSON body, which the queue shows as the row's
outcome.

### Outbox

Every write the device owes the server, in Room, authoritative:

```
outbox
  id           TEXT PRIMARY KEY   -- UUID
  kind         TEXT               -- capture_text | capture_files | done | snooze
  payload      TEXT               -- JSON: the text, or title/note, or moment id and until
  created_at   INTEGER
  attempts     INTEGER
  next_at      INTEGER            -- when the worker may try again
  state        TEXT               -- queued | sent | refused | held
  status       INTEGER            -- the HTTP status once sent
  answer       TEXT               -- the response body once sent
  error        TEXT               -- last failure, for the queue row
outbox_files
  outbox_id    TEXT
  path         TEXT               -- under filesDir/outbox/<id>/
  name         TEXT               -- the name the sender gave it
  mime         TEXT
```

`Outbox.enqueue(...)` copies every shared `content://` URI into
`filesDir/outbox/<id>/` **before** inserting the row and returns only once
both exist. A URI granted to an Activity is not readable an hour later, and an
hour later is when the VPN comes back. The caller is answered immediately.

`Outbox.rows: Flow<List<OutboxRow>>` is what the Queue screen draws.

State transitions, the only ones allowed:

- `queued → sent` on any 2xx; `status` and `answer` are recorded.
- `queued → queued` on a network failure or a 5xx; `attempts` increments and
  `next_at` moves out on the schedule 30 s, 2 min, 10 min, 30 min, 1 h, then
  every 2 h.
- `queued → held` on a 4xx other than 401: the server refused this row for
  what it is, and retrying will not change that. The row shows the server's
  message.
- `queued → refused` on 401: the credential is dead. Every queued row is
  moved to `refused` at once, sync stops, and the Refused banner appears.
  A re-pair moves `refused` rows back to `queued`.
- `sent` rows are deleted, with their files, after seven days.

A row's files are deleted when the row leaves `queued`, except a `held` row,
whose files stay so the person can see what was refused.

### Situation

The offer card on the web (`_context.html`, the anticipation layer) does not
know a device by its user agent. It knows it by `device_key` in
`core::context`, a hash over the stable fields of the bundle the browser posts
to `/ui/context`: platform, browser family, screen size, cores, memory,
language. The situation fields — time zone, colour scheme, orientation,
battery, charging, network — are encoded beside it, so one phone across a day
is one device in many situations.

A browser's bundle drifts under a hardened browser. The app's must not, and
it controls every field, so `Situation.bundle()` in `core` produces the same
JSON shape with the stable fields fixed by construction:

| Field | Value |
|---|---|
| `platform` | `Android` |
| `ua_family` | `engram-android` |
| `screen_w`, `screen_h` | the display's pixels, portrait order regardless of rotation |
| `cores` | `Runtime.availableProcessors()` |
| `memory_gb` | `ActivityManager.MemoryInfo.totalMem`, rounded to a half |
| `language` | the first system locale, as a BCP 47 tag |

and the situation fields from the platform each time it is asked: `tz` and
`tz_offset_mins` from `ZoneId.systemDefault()`, `color_scheme` from the night
mode configuration, `orientation`, `battery_level` and `charging` from
`BatteryManager`, `network` as `wifi`, `cellular` or `wired` from
`ConnectivityManager`, `touch` true, `dpr` from the display density, and
`languages` as the full locale list.

Part D builds it and tests that the stable half is identical across two calls
that differ in every situation field. Nothing in D posts it: the routes it
would go to answer HTML, and the card is a reading surface. Part E draws the
card on the app's home screen through the JSON door Part B gives it, and the
phone has been one stable device from the day it was paired.

### Cache

One table in D: `moments(id, title, at, fetched_at)`, written from each push
payload and read by the notification and by the Settings screen's *last
reminder* line. Part E grows this into the read models.

### Sync

`SyncWorker`, a `CoroutineWorker` with a unique name so two never run at once,
enqueued with a network constraint whenever the outbox gains a row and
scheduled again at the nearest `next_at` after each run. It drains `queued`
rows in `created_at` order, one attempt each per run, and stops at the first
`Refused`. A `PinMismatch` stops it too, and sets a flag the app renders as
the full-screen refusal.

### Push

`Push.register()` is called from the UnifiedPush `onNewEndpoint` callback: it
generates a P-256 keypair and a 16-byte auth secret, stores them next to the
connection, and `PUT`s endpoint, `p256dh` and `auth` to the server. `onUnregistered` clears them and `DELETE`s. Registration failures are
retried by a WorkManager job with the same schedule as the outbox; the
endpoint is not lost, only the delivery to the server.

`Push.decode(bytes): Payload` decrypts RFC 8291 `aes128gcm` with the stored
keys and parses the JSON. Version `1` yields `Due(at, moments, more)` or
`Notice(at, title, body)`. Any other `v` yields `Unknown(v)`, which still
rings: the notification says *something is due* and *update the app*. A
payload that fails to decrypt or parse is `Unknown(null)` and rings the same
way — a notification that fails to parse must still ring.

## `app`

### Screens

Four, on one `NavHost`:

- **Pair.** The camera preview with ZXing decoding, a *paste the code
  instead* field for a scanner that handed over text, and the device-name
  field prefilled `engram for Android <version> · <Build.MODEL>`. On a valid
  URI it claims, shows *pairing with <origin>* while it does, and lands on
  Compose. A server whose `v` is older than the app's minimum says so and
  offers to continue anyway.
- **Compose.** A text field, an attachment strip, a title and a note field
  folded away, and one button. Shares land here with their content already
  in the outbox and the screen confirming it; the person can add a note and
  the note is patched onto the queued row while it is still queued.
- **Queue.** Every outbox row, newest first: its first line or its file
  names, its state in words (*waiting*, *stored*, *already held*, *held for
  review*, *refused: <reason>*), and for a queued row the time of the next
  try. A held row can be deleted. A *deliver now* action runs the worker.
- **Settings.** The server and its version, the device name, the push
  status (registered with which distributor, or the UnifiedPush explanation
  and a link to the distributor list when none is installed), the last
  reminder received, *unpair* behind a confirmation.

Unpaired, the app shows Pair and nothing else. Refused, a banner on every
screen offers a rescan and Pair opens with the origin already known.

### The doors

All of them write to the outbox or to Connection and nothing else:

- `ACTION_SEND` and `ACTION_SEND_MULTIPLE`, for `text/plain`, `image/*`,
  `application/pdf` and `*/*`. Text with `EXTRA_TEXT` is a text capture; a
  single URL in the text is left to the server, which fetches it. Streams
  are copied and become a file capture.
- `ACTION_PROCESS_TEXT`, so a selection anywhere offers *Save to engram*.
  The capture is enqueued and the app finishes without opening a screen.
- A quick-settings tile and a launcher shortcut, both opening Compose empty.
- The `engram://` scheme, opening Pair with the URI.
- Notification actions *done* and *snooze one hour*, enqueued as outbox rows
  without opening the app.
- Camera and microphone straight into a capture, from Compose: a photo is
  a file capture, a recording is an audio file capture that the server
  transcribes.

### Notifications

One channel, *Reminders*. A `Due` payload renders one notification per push
with the moments as lines, a *done* action for the first moment, and a
*snooze* action for the first moment. `Notice` renders title and body.
`Unknown` renders *Something is due* with *This app is behind the server*.

### The theme

Copied, not approximated, from `assets/css/00-tokens.css`: the two palettes
as Compose colour schemes, the radii, the type scale. Inter 400/500/600 and
JetBrains Mono 400 from `assets/fonts/` as font resources. `wordmark.svg` as
a vector drawable for the top bar. No dynamic colour, no default Material
purple anywhere. `vbg.rs` is not ported in D.

### Failures, as the person sees them

- **Server unreachable.** Queue rows say *waiting, next try at …*. Nothing
  else changes. Nothing is lost.
- **Token revoked.** The Refused banner on every screen: *This phone was
  unpaired on the server. Scan a new code.* The queue holds.
- **Pin mismatch.** A full-screen refusal naming both fingerprints, with one
  action, *unpair*, and no way past it. This is the case the pin exists for.
- **No distributor.** Settings explains what UnifiedPush is, links the
  distributor list, and says reminders will not arrive until one is
  installed.

## Build

- Gradle 9.7, AGP 9.4, Kotlin 2.4, Compose BOM 2026.09.00, Room 2.8, Work
  2.11, OkHttp 5.5, ZXing 3.5, CameraX 1.6, UnifiedPush connector 3.3.
- `android/gradle.properties` is committed and names no JDK.
  `android/local.properties` and `android/gradle.properties.local` are
  ignored; the developer points `org.gradle.java.home` at a JDK 21 there.
  On this machine that is Android Studio's bundled runtime under
  `/var/lib/flatpak/app/com.google.AndroidStudio`.
- `.github/workflows/android.yml`: on any push or pull request touching
  `android/`, one job runs `./gradlew :core:test :app:assembleDebug` and
  uploads the APK; a second job runs `:core:connectedDebugAndroidTest` on a
  headless API 35 emulator.
- Release signing stays out of the repository. `release.yml` is not touched
  in D.

## Testing

JVM unit tests in `core`:

- `PairUri.parse` on the server's exact output (`o` percent-encoded, `f`
  absent and present), on a `http` non-loopback origin (null), on a foreign
  scheme (null), on a missing `c` (null).
- `Push.decode` on an encrypted version-1 `Due` and `Notice` produced by a
  Kotlin implementation of the RFC 8291 sender against the RFC's test
  vector; on `v: 2` (`Unknown(2)`); on garbage (`Unknown(null)`).
- Outbox transitions with a fake transport: 201 → sent with the body kept;
  IOException → queued with `next_at` on the schedule; 400 → held with the
  message; 401 → every queued row refused and the worker stopped.
- The backoff schedule, as a pure function of `attempts`.

Instrumentation tests in `core`, on a device or the CI emulator:

- enqueue while the transport is offline, then deliver on reconnect, with
  the file's bytes arriving intact;
- a row enqueued, the process killed, the worker started fresh: the row is
  delivered once.

No test on the phone asserts anything about ranking, result order, or what
the server did with a capture beyond its status code.

## Out of scope

Reading screens, the home-screen widget, the vector background, the watch,
the Play Store, a local alarm fallback, API token management in the app, and
any change to the server. The server side of D is Parts A and C, already on
the branch.
