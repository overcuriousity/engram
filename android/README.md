# engram for Android

Built apart from the server: `cd android && ./gradlew :app:assembleDebug`.
Needs an Android SDK (`local.properties` → `sdk.dir`) and a JDK 17 or newer.
JVM tests: `./gradlew :core:test`. Device tests: `./gradlew :core:connectedDebugAndroidTest`
with a phone attached or an emulator running.

The spec is `docs/superpowers/specs/2026-09-16-android-app-foundation-design.md`.
