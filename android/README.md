# engram for Android

Built apart from the server: `cd android && ./gradlew :app:assembleDebug`.
Needs an Android SDK (`local.properties` → `sdk.dir`) and a JDK 17 or newer.
JVM tests: `./gradlew :core:test`. Device tests: `./gradlew :core:connectedDebugAndroidTest`
with a phone attached or an emulator running.

The spec is `docs/superpowers/specs/2026-09-16-android-app-foundation-design.md`.

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
keytool -genkeypair -keystore engram.jks -alias engram \
  -keyalg RSA -keysize 4096 -validity 10000
base64 -w0 engram.jks   # → ANDROID_KEYSTORE_B64
```

Then set `ANDROID_KEYSTORE_B64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`
and `ANDROID_KEY_PASSWORD` under Settings → Secrets → Actions, and keep
`engram.jks` somewhere a lost laptop does not take with it. The release job
fails rather than publishing when the secrets are absent, because an unsigned
APK is one nothing will install and a release without an installable APK is an
update the phones cannot see.

Locally, `./gradlew :app:assembleRelease` needs none of this and produces an
unsigned APK; pointing `ENGRAM_KEYSTORE` at a keystore file, with
`ENGRAM_KEYSTORE_PASSWORD`, `ENGRAM_KEY_ALIAS` and `ENGRAM_KEY_PASSWORD` beside
it, signs it the way the workflow does.
