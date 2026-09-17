# engram for Android

Built apart from the server: `cd android && ./gradlew :app:assembleDebug`.
Needs an Android SDK (`local.properties` → `sdk.dir`) and a JDK 17 or newer.
JVM tests: `./gradlew :core:test`. Device tests: `./gradlew :core:connectedDebugAndroidTest`
with a phone attached or an emulator running.

The specs are `docs/superpowers/specs/2026-09-16-android-app-foundation-design.md`
(the foundation: capture, the outbox, notifications) and Parts E and F of
`docs/superpowers/specs/2026-09-08-android-companion-design.md` (reading, and
the decisions a person makes about the base).

## The screens

Search is home: a box, and beneath an empty one what the base has to say
unasked — the card offered for the situation the phone is in, what is due,
what is worth seeing again. A search runs when it is asked for rather than on
every keystroke; from a phone each keystroke would be an embedding call across
a VPN. Ask sits beside the box and streams, drawing the answer as it grows and
then drawing the server's whole answer in its place, with the commands and
paths no excerpt carries marked.

The bar holds Search, Capture, Today and Library. Queue and Settings are above,
and the queue shows a count only while something is still owed. Today is one
day of the base and pages by date; Library is everything captured, a page at a
time; an artifact shows its text, where it came from, how it came to exist, and
the wordings it has had.

The result list keeps what the web's keeps: the rule that says *relevance falls
off here*, and the rows beneath it that hold their rank and stop claiming to be
answers. A loose hit is badged rather than ranked. That is not decoration —
retrieval always returns its best candidates however bad they are, and a list
without the rule shows a typo exactly as it shows an answer.

## Judging, and why it is quiet

The decisions rather than the reads — which of two near-identical artifacts
stays, which questions the base could not answer are worth answering, what the
base did on its own while nobody was looking — live behind Settings. Not a tab,
not a section on home, and with no count anywhere until somebody has opened the
screen that fetched one.

That is on purpose and it is the whole design. This is the part of the system
meant to shrink: the human in the loop here is one to work towards removing,
not one to build a habit around. A queue of chores on the screen the app opens
on is an interface asking to be served, and an app that badges it teaches a
person to serve it.

A pair card offers all five answers the web offers — keep either side, write
one from both, discard both, dismiss — and says only what somebody actually
established about the pair. Where the sweep filed a pair on a score and nothing
has read it since, the card prints the measurement and no finding, because
*these two cover the same ground* is a finding nobody made. Where a pair came
from repeated co-retrieval no similarity was ever computed, so no percentage is
shown. Where a merge would be refused, there is no button to press.

What a set-aside row admits is decided by its `kind` and nothing else — never
by reading its wording — so a `kind` this build has never heard of draws no
buttons and is still shown. What the base did is worth knowing even where this
app cannot answer it.

Every answer is an outbox row, like every other write the device owes: a
decision made on a train is a decision. It is also where undo comes from. Until
the row is delivered, taking an answer back is deleting a row — not a second
write undoing the first, which would be a different and less honest thing.

## When the server cannot be reached

Nothing is fetched that nobody asked for, so what a screen can show without the
server is what was opened on this phone before. When a read fails the screen
says `Server unreachable`, with when the content was fetched and a retry, above
whatever was held — or above nothing. Writes are different: a capture, a Done,
a snooze go to the outbox and are delivered when the server can be reached
again.

Every screen asks `Reader` in `core` and never learns where the answer came
from. That is deliberate: a later version of this app is meant to be
self-contained, an engram on the device replacing the server by default, and it
is a second implementation of that one interface rather than a rewrite of the
screens.

## Tests

`./gradlew lintDebug testDebugUnitTest :app:assembleDebug` is the whole of what
runs without a device. Beyond the plain unit tests:

- `-Pengram.pictures=1` makes `PicturesTest` write PNGs of the screens' parts to
  `app/build/pictures/`, for looking at while no phone is in the loop.
- `-Pengram.live.origin=… -Pengram.live.token=…` points `LiveServerTest` at a
  running engram and drives the reader and Ask against it over real HTTP. It
  skips itself when they are absent. The token is a device token: pair the way
  a phone does, or mint one under Settings → API tokens.
- `core/src/test/resources/api/*.json` are not written by hand. The Rust test
  `src/web/android_fixtures.rs` produces them from the real routes and checks
  on every server test run that they still have the shape the server answers
  with, so a field renamed on the server fails a test that names the file.

## Installing

Every release carries a signed APK, named `engram-<version>.apk`, beside the
server binaries: <https://github.com/overcuriousity/engram/releases>. It can be
downloaded and installed by hand, but the point of publishing it that way is
[Obtainium](https://github.com/ImranR98/Obtainium), which watches a GitHub
repository's releases and offers each new one as an update. Add
`https://github.com/overcuriousity/engram` as an app, take the defaults, and
the phone follows the same version the server does — there is one APK for every
architecture, so nothing needs filtering.

A release is cut on nearly every push to master, and most of them change the
server rather than the app. The APK is rebuilt for each one regardless, so
Obtainium will offer an update most days, often with no change to the app in
it.

## Pairing

On the web, Settings → Pair the app draws a code. In the app, scan it (or
paste the `engram://pair?…` text). The token then appears under Settings →
API tokens on the web, named for the phone.

## Reminders

They arrive over UnifiedPush and need a distributor on the phone: ntfy or
NextPush, both on F-Droid. Settings → Register for reminders picks one.

## CI

`.github/workflows/android.yml` builds the debug APK and runs the JVM tests on
every push touching `android/`, and runs the device tests on a headless
emulator in a second job.

The release itself is built by the `android` job in
`.github/workflows/release.yml`, which runs `:app:assembleRelease` and attaches
the APK to the release the server binaries go to. `versionName` is the release's
CalVer and `versionCode` is that same date packed into an integer
(`2026.917.0` → `26091700`), which is what lets a phone tell one build from the
next.

## The signing key

Android identifies an app by its signature, so every engram APK ever published
has to be signed by the same key. A build signed by a different one will not
install over an older build at all: the person has to uninstall first, which
throws away the pairing. Losing the key is therefore not a thing that can be
recovered from by making a new one — it ends that install base.

The key lives in four repository secrets and nowhere else. To mint it:

```sh
# Outside the repository: a signing key in the tree is a signing key one
# `git add -A` away from being public. `-dname` is there to skip keytool's
# certificate questionnaire, which nothing ever reads for an app key.
keytool -genkeypair -keystore ~/engram.jks -alias engram \
  -keyalg RSA -keysize 4096 -validity 10000 -dname "CN=engram"
base64 -w0 ~/engram.jks   # → ANDROID_KEYSTORE_B64
```

Then set `ANDROID_KEYSTORE_B64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`
and `ANDROID_KEY_PASSWORD` under Settings → Secrets → Actions, and keep
`engram.jks` somewhere a lost laptop does not take with it. A keystore made
this way is PKCS12, where keytool holds the key password and the store password
to the same value — so the two password secrets take the same string. The release job
fails rather than publishing when the secrets are absent, because an unsigned
APK is one nothing will install and a release without an installable APK is an
update the phones cannot see.

Locally, `./gradlew :app:assembleRelease` needs none of this and produces an
unsigned APK; pointing `ENGRAM_KEYSTORE` at a keystore file, with
`ENGRAM_KEYSTORE_PASSWORD`, `ENGRAM_KEY_ALIAS` and `ENGRAM_KEY_PASSWORD` beside
it, signs it the way the workflow does.
