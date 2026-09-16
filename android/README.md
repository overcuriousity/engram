# engram for Android

Built apart from the server: `cd android && ./gradlew :app:assembleDebug`.
Needs an Android SDK (`local.properties` → `sdk.dir`) and a JDK 17 or newer.
JVM tests: `./gradlew :core:test`. Device tests: `./gradlew :core:connectedDebugAndroidTest`
with a phone attached or an emulator running.

The spec is `docs/superpowers/specs/2026-09-16-android-app-foundation-design.md`.

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
