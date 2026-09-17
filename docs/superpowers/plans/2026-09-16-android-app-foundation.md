# Android App Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The first installable engram app for Android: pairs by scanning the QR from `/ui/app`, captures from every door the phone offers into an outbox that survives anything, and rings for reminders over UnifiedPush.

**Architecture:** Two Gradle modules under `android/`. `core` is an Android library holding Connection, Transport (internal), Outbox, Cache, Sync, Push and Situation, with no UI type in it; `app` holds the one Activity, four Compose screens, the receivers, the tile, the UnifiedPush service, the notification and a theme copied from the web's tokens. Screens and receivers write to the outbox and read from Room; only the sync worker talks to the server.

**Tech Stack:** Kotlin 2.4, AGP 9.4, Gradle 9.7, Compose BOM 2026.09.00, Room 2.8.5 on the bundled SQLite driver (so DAO tests run on the JVM), WorkManager 2.11.2, OkHttp 5.5.0 with mockwebserver3 in tests, kotlinx.serialization, CameraX 1.6.2 + ZXing 3.5.4, UnifiedPush connector 3.3.5.

**Spec:** `docs/superpowers/specs/2026-09-16-android-app-foundation-design.md`. Read the whole spec first; the *Situation* section's web half is the separate plan `2026-09-16-situation-vocabulary.md`, which must be done first (it creates `bundle-fields.txt`).

## Global Constraints

- Branch: `feat/web-push`. Never rebase or touch existing commits.
- Application id `io.github.overcuriousity.engram`; `minSdk 29`, `compileSdk 37`, `targetSdk 37`.
- Nothing from Google Play Services. ZXing for QR, UnifiedPush for push.
- `Transport` is `internal` to `core`. Nothing in `app` calls the server.
- `User-Agent: engram-android/<versionName> (<Build.MODEL>)` on every request.
- Outbox states: `queued | sent | refused | held`. Backoff: 30 s, 2 min, 10 min, 30 min, 1 h, then 2 h.
- Pairing URI: `engram://pair?o=…&c=…&v=…[&f=…]`; `f` is 43 chars base64url. `http` origin only for a loopback host.
- Payload version 1: `{"v":1,"kind":"due","at":…,"moments":[{"id","title","at"}],"more":n}` or `{"v":1,"kind":"notice","at":…,"title","body"}`. Any other `v`, or unparseable, is `Unknown` and still rings.
- UI copy: a term and a short gloss, never an explanatory sentence.
- Theme colours and type from `assets/css/00-tokens.css`; no Material dynamic colour.
- Build with the system JDK 25 (`/usr/bin/java`); `sdk.dir=/home/user01/Android/Sdk` in `android/local.properties` (ignored by git).
- Every commit message ends with the evidence line (which Gradle task, how many tests) and `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.
- JVM tests: `cd android && ./gradlew :core:test`. Instrumentation tests need a device or the CI emulator; write them, run them when one is attached, and say in the commit if they were not run.
- No test asserts anything about ranking or result order.

## File map

```
android/
  settings.gradle.kts, build.gradle.kts, gradle.properties, gradle/libs.versions.toml, gradlew, gradle/wrapper/
  README.md
  core/
    build.gradle.kts, src/main/AndroidManifest.xml
    src/main/kotlin/io/github/overcuriousity/engram/core/
      Engram.kt              — the façade: builds everything once from a Context
      Connection.kt          — Connection, ConnectionStore, SecretBox (+KeystoreBox)
      PairUri.kt             — the parser
      Transport.kt           — internal OkHttp client, Refused, PinMismatch, Api calls
      Pairing.kt             — claim(), trust-on-first-use
      Situation.kt           — Situation, SituationSource, the bundle
      db/Db.kt               — RoomDatabase, entities, DAOs
      outbox/Outbox.kt       — enqueue, rows, transitions
      outbox/Backoff.kt      — the schedule
      outbox/Drainer.kt      — one pass over queued rows (pure, testable)
      sync/SyncWorker.kt     — CoroutineWorker around Drainer, scheduling
      push/Payload.kt        — parse
      push/Push.kt           — registration with the server, decode entry
    src/test/kotlin/…        — JVM tests; src/test/resources/bundle-fields.txt
    src/androidTest/kotlin/… — outbox on a device
  app/
    build.gradle.kts, src/main/AndroidManifest.xml, res/
    src/main/kotlin/io/github/overcuriousity/engram/
      App.kt                 — Application: holds Engram
      MainActivity.kt        — NavHost, banners
      ui/Theme.kt            — colours, type, shapes
      ui/PairScreen.kt, ui/ComposeScreen.kt, ui/QueueScreen.kt, ui/SettingsScreen.kt
      ui/Scanner.kt          — CameraX + ZXing composable
      doors/ShareActivity.kt — ACTION_SEND, SEND_MULTIPLE, PROCESS_TEXT
      doors/CaptureTile.kt   — quick-settings tile
      push/PushServiceImpl.kt, push/Reminders.kt (notification), push/ActionReceiver.kt
```

---

### Task 1: The Gradle scaffold builds and runs one JVM test

**Files:**
- Create: `android/settings.gradle.kts`, `android/build.gradle.kts`, `android/gradle.properties`, `android/gradle/libs.versions.toml`, `android/core/build.gradle.kts`, `android/core/src/main/AndroidManifest.xml`, `android/app/build.gradle.kts`, `android/app/src/main/AndroidManifest.xml`, `android/README.md`
- Create: `android/core/src/test/kotlin/io/github/overcuriousity/engram/core/ScaffoldTest.kt`
- Create: `android/app/src/main/kotlin/io/github/overcuriousity/engram/App.kt`, `MainActivity.kt` (empty shells)
- Modify: `.gitignore` (append), `android/.gitignore`

**Interfaces:**
- Produces: the module layout every later task adds to; the version catalog aliases used below.

- [ ] **Step 1: Wrapper**

Gradle is not installed. Fetch the wrapper from the distribution once:

```bash
mkdir -p android/gradle/wrapper && cd android
curl -sLo /tmp/claude-1000/gradle.zip https://services.gradle.org/distributions/gradle-9.7.1-bin.zip
unzip -q /tmp/claude-1000/gradle.zip -d /tmp/claude-1000/
/tmp/claude-1000/gradle-9.7.1/bin/gradle wrapper --gradle-version 9.7.1 --distribution-type bin
ls gradlew gradle/wrapper/gradle-wrapper.jar gradle/wrapper/gradle-wrapper.properties
```

(Use the session scratchpad path instead of `/tmp/claude-1000` if it differs.)

- [ ] **Step 2: Catalog and settings**

`android/gradle/libs.versions.toml`:

```toml
[versions]
agp = "9.4.0"
kotlin = "2.4.20"
ksp = "2.3.12"
compose-bom = "2026.09.00"
room = "2.8.5"
sqlite = "2.7.1"
work = "2.11.2"
okhttp = "5.5.0"
serialization = "1.11.0"
coroutines = "1.11.0"
camerax = "1.6.2"
zxing = "3.5.4"
unifiedpush = "3.3.5"
tink = "1.23.0"
navigation = "2.10.1"
activity = "1.13.0"
lifecycle = "2.11.0"
core-ktx = "1.19.0"
junit = "4.13.2"
androidx-test = "1.7.0"
androidx-junit = "1.3.0"

[libraries]
core-ktx = { module = "androidx.core:core-ktx", version.ref = "core-ktx" }
coroutines-android = { module = "org.jetbrains.kotlinx:kotlinx-coroutines-android", version.ref = "coroutines" }
coroutines-test = { module = "org.jetbrains.kotlinx:kotlinx-coroutines-test", version.ref = "coroutines" }
serialization-json = { module = "org.jetbrains.kotlinx:kotlinx-serialization-json", version.ref = "serialization" }
okhttp = { module = "com.squareup.okhttp3:okhttp", version.ref = "okhttp" }
mockwebserver = { module = "com.squareup.okhttp3:mockwebserver3", version.ref = "okhttp" }
room-runtime = { module = "androidx.room:room-runtime", version.ref = "room" }
room-compiler = { module = "androidx.room:room-compiler", version.ref = "room" }
sqlite-bundled = { module = "androidx.sqlite:sqlite-bundled", version.ref = "sqlite" }
work-runtime = { module = "androidx.work:work-runtime-ktx", version.ref = "work" }
unifiedpush = { module = "org.unifiedpush.android:connector", version.ref = "unifiedpush" }
tink-android = { module = "com.google.crypto.tink:tink-android", version.ref = "tink" }
compose-bom = { module = "androidx.compose:compose-bom", version.ref = "compose-bom" }
compose-ui = { module = "androidx.compose.ui:ui" }
compose-material3 = { module = "androidx.compose.material3:material3" }
compose-tooling-preview = { module = "androidx.compose.ui:ui-tooling-preview" }
compose-tooling = { module = "androidx.compose.ui:ui-tooling" }
activity-compose = { module = "androidx.activity:activity-compose", version.ref = "activity" }
navigation-compose = { module = "androidx.navigation:navigation-compose", version.ref = "navigation" }
lifecycle-runtime-compose = { module = "androidx.lifecycle:lifecycle-runtime-compose", version.ref = "lifecycle" }
camerax-core = { module = "androidx.camera:camera-core", version.ref = "camerax" }
camerax-camera2 = { module = "androidx.camera:camera-camera2", version.ref = "camerax" }
camerax-lifecycle = { module = "androidx.camera:camera-lifecycle", version.ref = "camerax" }
camerax-view = { module = "androidx.camera:camera-view", version.ref = "camerax" }
zxing = { module = "com.google.zxing:core", version.ref = "zxing" }
junit = { module = "junit:junit", version.ref = "junit" }
androidx-test-runner = { module = "androidx.test:runner", version.ref = "androidx-test" }
androidx-test-junit = { module = "androidx.test.ext:junit", version.ref = "androidx-junit" }
work-testing = { module = "androidx.work:work-testing", version.ref = "work" }

[plugins]
android-application = { id = "com.android.application", version.ref = "agp" }
android-library = { id = "com.android.library", version.ref = "agp" }
kotlin-android = { id = "org.jetbrains.kotlin.android", version.ref = "kotlin" }
kotlin-compose = { id = "org.jetbrains.kotlin.plugin.compose", version.ref = "kotlin" }
kotlin-serialization = { id = "org.jetbrains.kotlin.plugin.serialization", version.ref = "kotlin" }
ksp = { id = "com.google.devtools.ksp", version.ref = "ksp" }
```

If a version does not resolve, take the newest stable from the metadata at `https://dl.google.com/dl/android/maven2/<group path>/<artifact>/maven-metadata.xml` or `https://repo1.maven.org/maven2/<group path>/<artifact>/maven-metadata.xml`, note it in the commit, and move on. `kotlinx-serialization` in particular may need `1.10.x` if `1.11.0` is not there.

`android/settings.gradle.kts`:

```kotlin
pluginManagement {
    repositories { google(); mavenCentral(); gradlePluginPortal() }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories { google(); mavenCentral() }
}
rootProject.name = "engram"
include(":core", ":app")
```

`android/build.gradle.kts`:

```kotlin
plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.android.library) apply false
    alias(libs.plugins.kotlin.android) apply false
    alias(libs.plugins.kotlin.compose) apply false
    alias(libs.plugins.kotlin.serialization) apply false
    alias(libs.plugins.ksp) apply false
}
```

`android/gradle.properties`:

```properties
org.gradle.jvmargs=-Xmx3g
org.gradle.caching=true
org.gradle.configuration-cache=true
android.useAndroidX=true
kotlin.code.style=official
```

`android/.gitignore`:

```
.gradle/
build/
local.properties
*.iml
.idea/
.kotlin/
```

Append to the repository `.gitignore`:

```
# The Android app's own build output and machine-local SDK path.
android/.gradle/
android/**/build/
android/local.properties
android/.kotlin/
```

`android/local.properties` (not committed): `sdk.dir=/home/user01/Android/Sdk`.

- [ ] **Step 3: `core` module**

`android/core/build.gradle.kts`:

```kotlin
plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.serialization)
    alias(libs.plugins.ksp)
}

android {
    namespace = "io.github.overcuriousity.engram.core"
    compileSdk = 37
    defaultConfig {
        minSdk = 29
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    testOptions.unitTests.isReturnDefaultValues = true
}

kotlin { jvmToolchain(17) }

room { schemaDirectory("$projectDir/schemas") }

dependencies {
    implementation(libs.core.ktx)
    implementation(libs.coroutines.android)
    implementation(libs.serialization.json)
    implementation(libs.okhttp)
    implementation(libs.room.runtime)
    implementation(libs.sqlite.bundled)
    ksp(libs.room.compiler)
    implementation(libs.work.runtime)
    implementation(libs.unifiedpush)

    testImplementation(libs.junit)
    testImplementation(libs.coroutines.test)
    testImplementation(libs.mockwebserver)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.junit)
    androidTestImplementation(libs.work.testing)
}
```

`room { … }` needs the Room Gradle plugin; add to the catalog `room-plugin = { id = "androidx.room", version.ref = "room" }`, to the root `alias(libs.plugins.room.plugin) apply false`, and to core `alias(libs.plugins.room.plugin)`. If `jvmToolchain(17)` cannot provision a 17 on this machine, replace it with `jvmToolchain(25)` and `JavaVersion.VERSION_25` in both modules — AGP 9.4 compiles against either; the point is only that both modules agree.

`android/core/src/main/AndroidManifest.xml`:

```xml
<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <uses-permission android:name="android.permission.INTERNET" />
</manifest>
```

- [ ] **Step 4: `app` module**

`android/app/build.gradle.kts`:

```kotlin
plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
}

android {
    namespace = "io.github.overcuriousity.engram"
    compileSdk = 37
    defaultConfig {
        applicationId = "io.github.overcuriousity.engram"
        minSdk = 29
        targetSdk = 37
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }
    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures { compose = true }
}

kotlin { jvmToolchain(17) }

// The connector depends on tink; pin the Android artefact so no other
// dependency drags the JVM one in beside it (duplicate classes otherwise).
configurations.configureEach {
    resolutionStrategy {
        force(libs.tink.android.get().toString())
        dependencySubstitution {
            substitute(module("com.google.crypto.tink:tink")).using(module(libs.tink.android.get().toString()))
        }
    }
}

dependencies {
    implementation(project(":core"))
    implementation(libs.core.ktx)
    implementation(libs.coroutines.android)
    implementation(libs.serialization.json)
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.material3)
    implementation(libs.compose.tooling.preview)
    debugImplementation(libs.compose.tooling)
    implementation(libs.activity.compose)
    implementation(libs.navigation.compose)
    implementation(libs.lifecycle.runtime.compose)
    implementation(libs.work.runtime)
    implementation(libs.unifiedpush)
    implementation(libs.camerax.core)
    implementation(libs.camerax.camera2)
    implementation(libs.camerax.lifecycle)
    implementation(libs.camerax.view)
    implementation(libs.zxing)
    testImplementation(libs.junit)
}
```

`android/app/proguard-rules.pro`: empty file with one comment line.

`android/app/src/main/AndroidManifest.xml` (Task 12 and 13 add to it):

```xml
<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <uses-permission android:name="android.permission.INTERNET" />
    <application
        android:name=".App"
        android:label="engram"
        android:icon="@mipmap/ic_launcher"
        android:theme="@android:style/Theme.Material.NoActionBar"
        android:supportsRtl="true">
        <activity android:name=".MainActivity" android:exported="true" android:launchMode="singleTask">
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>
```

Launcher icon: convert `assets/icon.svg` with Android Studio's Image Asset tool later; for now put `assets/icon-192.png` as `res/mipmap-xxxhdpi/ic_launcher.png` (a copy, `cp`), and the same file in `mipmap-hdpi`, `-xhdpi`, `-xxhdpi`.

`App.kt`:

```kotlin
package io.github.overcuriousity.engram

import android.app.Application

class App : Application()
```

`MainActivity.kt`:

```kotlin
package io.github.overcuriousity.engram

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.material3.Text

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent { Text("engram") }
    }
}
```

- [ ] **Step 5: One JVM test**

`android/core/src/test/kotlin/io/github/overcuriousity/engram/core/ScaffoldTest.kt`:

```kotlin
package io.github.overcuriousity.engram.core

import org.junit.Assert.assertTrue
import org.junit.Test

class ScaffoldTest {
    @Test fun theFixtureIsOnTheClasspath() {
        val text = javaClass.getResource("/bundle-fields.txt")!!.readText()
        assertTrue(text.lines().contains("tz"))
    }
}
```

- [ ] **Step 6: Build**

```bash
cd android && ./gradlew :core:test :app:assembleDebug --console=plain 2>&1 | tail -20
```

Expected: `BUILD SUCCESSFUL`, `app/build/outputs/apk/debug/app-debug.apk` exists. The first run downloads ~1 GB of dependencies. Fix version resolution errors per Step 2 before anything else.

- [ ] **Step 7: README and commit**

`android/README.md`:

```markdown
# engram for Android

Built apart from the server: `cd android && ./gradlew :app:assembleDebug`.
Needs an Android SDK (`local.properties` → `sdk.dir`) and a JDK 17 or newer.
JVM tests: `./gradlew :core:test`. Device tests: `./gradlew :core:connectedDebugAndroidTest`
with a phone attached or an emulator running.

The spec is `docs/superpowers/specs/2026-09-16-android-app-foundation-design.md`.
```

```bash
git add android .gitignore
git commit -m "feat(android): the Gradle scaffold — core and app modules, one JVM test

Evidence: ./gradlew :core:test :app:assembleDebug — BUILD SUCCESSFUL, 1 test.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: `PairUri` parses what the server draws

**Files:**
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/PairUri.kt`
- Test: `android/core/src/test/kotlin/io/github/overcuriousity/engram/core/PairUriTest.kt`

**Interfaces:**
- Produces: `data class PairUri(val origin: String, val code: String, val serverVersion: String, val fingerprint: String?)` and `PairUri.parse(text: String): PairUri?`.

- [ ] **Step 1: Failing tests**

```kotlin
package io.github.overcuriousity.engram.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PairUriTest {
    // Exactly what `pair_uri` in src/web/app.rs writes.
    private val server = "engram://pair?o=https%3A%2F%2Fengram.test&c=abc-DEF_123&v=0.1.0"

    @Test fun theServersOwnOutputParses() {
        val p = PairUri.parse(server)!!
        assertEquals("https://engram.test", p.origin)
        assertEquals("abc-DEF_123", p.code)
        assertEquals("0.1.0", p.serverVersion)
        assertNull(p.fingerprint)
    }

    @Test fun aFingerprintRidesAlong() {
        val f = "A".repeat(43)
        assertEquals(f, PairUri.parse("$server&f=$f")!!.fingerprint)
    }

    @Test fun aFingerprintOfTheWrongLengthIsRefused() {
        assertNull(PairUri.parse("$server&f=short"))
    }

    @Test fun aTrailingSlashOnTheOriginIsDropped() {
        assertEquals("https://engram.test", PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test%2F&c=x&v=1")!!.origin)
    }

    @Test fun plainHttpIsOnlyForLoopback() {
        assertNull(PairUri.parse("engram://pair?o=http%3A%2F%2Fengram.test&c=x&v=1"))
        assertEquals("http://127.0.0.1:8080", PairUri.parse("engram://pair?o=http%3A%2F%2F127.0.0.1%3A8080&c=x&v=1")!!.origin)
        assertEquals("http://localhost:8080", PairUri.parse("engram://pair?o=http%3A%2F%2Flocalhost%3A8080&c=x&v=1")!!.origin)
    }

    @Test fun anOriginWithAPathIsRefused() {
        assertNull(PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test%2Fui&c=x&v=1"))
    }

    @Test fun anythingElseIsNull() {
        assertNull(PairUri.parse("https://engram.test/ui/app"))
        assertNull(PairUri.parse("engram://other?o=https%3A%2F%2Fengram.test&c=x&v=1"))
        assertNull(PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test&v=1"))
        assertNull(PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test&c=&v=1"))
        assertNull(PairUri.parse("engram://pair?o=https%3A%2F%2Fengram.test&c=x"))
        assertNull(PairUri.parse(""))
    }
}
```

- [ ] **Step 2: Run, expect failure**

Run: `cd android && ./gradlew :core:test --tests '*PairUriTest*'`
Expected: compile error, unresolved `PairUri`.

- [ ] **Step 3: Implement**

```kotlin
package io.github.overcuriousity.engram.core

import java.net.URI
import java.net.URLDecoder

/**
 * What the QR on `/ui/app` carries. Parsed strictly: a scanner hands over
 * whatever it saw, and the one thing this must never do is claim against an
 * origin the server did not name.
 */
data class PairUri(
    val origin: String,
    val code: String,
    val serverVersion: String,
    val fingerprint: String?,
) {
    companion object {
        private val B64URL_43 = Regex("^[A-Za-z0-9_-]{43}$")

        fun parse(text: String): PairUri? {
            val t = text.trim()
            if (!t.startsWith("engram://pair?")) return null
            val q = t.removePrefix("engram://pair?")
                .split('&')
                .mapNotNull { kv ->
                    val i = kv.indexOf('=')
                    if (i < 0) null else kv.substring(0, i) to URLDecoder.decode(kv.substring(i + 1), "UTF-8")
                }
                .toMap()
            val code = q["c"]?.takeIf { it.isNotEmpty() } ?: return null
            val version = q["v"]?.takeIf { it.isNotEmpty() } ?: return null
            val origin = normaliseOrigin(q["o"] ?: return null) ?: return null
            val f = q["f"]
            if (f != null && !B64URL_43.matches(f)) return null
            return PairUri(origin, code, version, f)
        }

        /** `scheme://host[:port]`, and nothing else. */
        private fun normaliseOrigin(raw: String): String? {
            val u = runCatching { URI(raw) }.getOrNull() ?: return null
            val host = u.host ?: return null
            if (u.userInfo != null || u.query != null || u.fragment != null) return null
            if (u.rawPath != "" && u.rawPath != "/") return null
            val loopback = host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]"
            when (u.scheme) {
                "https" -> {}
                "http" -> if (!loopback) return null
                else -> return null
            }
            val port = if (u.port == -1) "" else ":${u.port}"
            return "${u.scheme}://$host$port"
        }
    }
}
```

- [ ] **Step 4: Run, expect pass; commit**

Run: `./gradlew :core:test --tests '*PairUriTest*'` → 7 passed.

```bash
git add android/core
git commit -m "feat(android): PairUri parses the QR the server draws, and nothing else

Evidence: ./gradlew :core:test --tests '*PairUriTest*' — 7 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: `Connection` and its store

**Files:**
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/Connection.kt`
- Test: `android/core/src/test/kotlin/io/github/overcuriousity/engram/core/ConnectionStoreTest.kt`

**Interfaces:**
- Produces:
  - `data class Connection(origin, token, pin: String?, serverVersion, deviceName)` — `@Serializable`.
  - `interface SecretBox { fun seal(plain: ByteArray): ByteArray; fun open(sealed: ByteArray): ByteArray }`; `class KeystoreBox(alias: String = "engram-connection") : SecretBox` (AES-GCM, key in AndroidKeyStore); `class PlainBox : SecretBox` for tests.
  - `class ConnectionStore(private val file: File, private val box: SecretBox)` with `val current: StateFlow<Connection?>`, `fun set(c: Connection)`, `fun clear()`, plus `var pushKeys: PushKeys?` stored the same way (`@Serializable data class PushKeys(endpoint, p256dh, auth, distributor)`).

The spec named `EncryptedSharedPreferences`; that library is deprecated, so this is the same thing by hand: one file, one Keystore key, AES-GCM. Both stores are a JSON blob sealed by the box, so the JVM test uses `PlainBox` and the Keystore path is exercised on a device in Task 16.

- [ ] **Step 1: Failing tests**

```kotlin
package io.github.overcuriousity.engram.core

import org.junit.Assert.*
import org.junit.Test
import java.io.File
import kotlin.io.path.createTempDirectory

class ConnectionStoreTest {
    private val dir = createTempDirectory("engram").toFile()
    private fun store() = ConnectionStore(File(dir, "connection"), PlainBox())
    private val c = Connection("https://engram.test", "engram_abc", null, "0.1.0", "engram for Android 0.1.0 · Pixel 8")

    @Test fun startsUnpaired() { assertNull(store().current.value) }

    @Test fun setThenReadBackAcrossInstances() {
        store().set(c)
        assertEquals(c, store().current.value)
    }

    @Test fun clearForgetsTheConnectionAndTheKeys() {
        val s = store()
        s.set(c)
        s.pushKeys = PushKeys("https://push.test/x", "BP…", "auth", "org.example.distributor")
        s.clear()
        assertNull(store().current.value)
        assertNull(store().pushKeys)
    }

    @Test fun theFileIsNotPlaintextWhenTheBoxSeals() {
        val s = ConnectionStore(File(dir, "sealed"), object : SecretBox {
            override fun seal(plain: ByteArray) = plain.map { (it.toInt() xor 0x5a).toByte() }.toByteArray()
            override fun open(sealed: ByteArray) = seal(sealed)
        })
        s.set(c)
        assertFalse(File(dir, "sealed").readText(Charsets.ISO_8859_1).contains("engram_abc"))
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**

- [ ] **Step 3: Implement**

```kotlin
package io.github.overcuriousity.engram.core

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** What the phone knows about its server. `null` in the store means unpaired. */
@Serializable
data class Connection(
    val origin: String,
    val token: String,
    /** SPKI SHA-256, base64url, or null when the chain was publicly trusted. */
    val pin: String?,
    val serverVersion: String,
    val deviceName: String,
)

/** The UnifiedPush registration as the server knows it. */
@Serializable
data class PushKeys(val endpoint: String, val p256dh: String, val auth: String, val distributor: String)

interface SecretBox {
    fun seal(plain: ByteArray): ByteArray
    fun open(sealed: ByteArray): ByteArray
}

/** Tests only: nothing sealed. */
class PlainBox : SecretBox {
    override fun seal(plain: ByteArray) = plain
    override fun open(sealed: ByteArray) = sealed
}

/** AES-GCM under a key that never leaves the Keystore. 12-byte IV prefixed. */
class KeystoreBox(private val alias: String = "engram-connection") : SecretBox {
    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(alias, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return gen.generateKey()
    }

    override fun seal(plain: ByteArray): ByteArray {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key())
        return c.iv + c.doFinal(plain)
    }

    override fun open(sealed: ByteArray): ByteArray {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, sealed, 0, 12))
        return c.doFinal(sealed, 12, sealed.size - 12)
    }
}

@Serializable
private data class Stored(val connection: Connection? = null, val pushKeys: PushKeys? = null)

/**
 * One sealed file. Small enough to rewrite whole on every change, which is
 * what makes "set" and "clear" atomic: write beside, rename over.
 */
class ConnectionStore(private val file: File, private val box: SecretBox) {
    private val json = Json { ignoreUnknownKeys = true; encodeDefaults = true }
    private var stored: Stored = load()
    private val _current = MutableStateFlow(stored.connection)
    val current: StateFlow<Connection?> get() = _current

    var pushKeys: PushKeys?
        get() = stored.pushKeys
        set(v) { stored = stored.copy(pushKeys = v); save() }

    fun set(c: Connection) { stored = stored.copy(connection = c); save(); _current.value = c }

    fun clear() { stored = Stored(); save(); _current.value = null }

    private fun load(): Stored =
        if (!file.exists()) Stored()
        else runCatching { json.decodeFromString<Stored>(String(box.open(file.readBytes()))) }.getOrDefault(Stored())

    private fun save() {
        val tmp = File(file.path + ".tmp")
        tmp.writeBytes(box.seal(json.encodeToString(Stored.serializer(), stored).toByteArray()))
        tmp.renameTo(file)
    }
}
```

- [ ] **Step 4: Run, expect 4 passed; commit**

```bash
git add android/core
git commit -m "feat(android): Connection, sealed in one Keystore-wrapped file

Evidence: ./gradlew :core:test --tests '*ConnectionStoreTest*' — 4 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: `Transport` and `Pairing.claim`

**Files:**
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/Transport.kt`
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/Pairing.kt`
- Test: `android/core/src/test/kotlin/io/github/overcuriousity/engram/core/TransportTest.kt`, `PairingTest.kt`

**Interfaces:**
- Produces (all `internal` except the exceptions and `Pairing`):
  - `class Refused : IOException("the server refused the credential")`
  - `class PinMismatch(val expected: String, val served: String) : IOException(...)`
  - `internal class Transport(val connection: Connection, val userAgent: String, client: OkHttpClient? = null)` with
    - `suspend fun captureText(text: String, title: String?, note: String?, tz: String): Answer`
    - `suspend fun captureFiles(files: List<OutFile>, title: String?, note: String?, tz: String): Answer`
    - `suspend fun vapid(): String`
    - `suspend fun registerPush(endpoint: String, p256dh: String, auth: String)`
    - `suspend fun unregisterPush()`
    - `suspend fun momentDone(id: String)`; `suspend fun momentSnooze(id: String, until: Long)`
  - `data class Answer(val status: Int, val body: String)`; `data class OutFile(val path: String, val name: String, val mime: String)`
  - `object Pairing { suspend fun claim(uri: PairUri, deviceName: String, userAgent: String): Connection }` and `class ClaimRefused : IOException(...)`
  - `fun userAgent(versionName: String, model: String) = "engram-android/$versionName ($model)"`

- [ ] **Step 1: Failing tests for Transport**

```kotlin
package io.github.overcuriousity.engram.core

import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test

class TransportTest {
    private val server = MockWebServer()
    private lateinit var t: Transport

    @Before fun up() {
        server.start()
        val c = Connection(server.url("/").toString().trimEnd('/'), "engram_tok", null, "0.1.0", "dev")
        t = Transport(c, "engram-android/0.1.0 (Test)")
    }
    @After fun down() = server.close()

    @Test fun everyRequestCarriesBearerAndUserAgent() = runTest {
        server.enqueue(MockResponse(code = 201, body = """{"id":"a1"}"""))
        val a = t.captureText("hello", null, null, "Europe/Berlin")
        assertEquals(201, a.status)
        assertEquals("""{"id":"a1"}""", a.body)
        val r = server.takeRequest()
        assertEquals("Bearer engram_tok", r.headers["Authorization"])
        assertEquals("engram-android/0.1.0 (Test)", r.headers["User-Agent"])
        assertEquals("/api/v1/capture?tz=Europe%2FBerlin", r.target)
        assertTrue(r.headers["Content-Type"]!!.startsWith("text/plain"))
        assertEquals("hello", r.body!!.utf8())
    }

    @Test fun titleAndNoteRideTheQuery() = runTest {
        server.enqueue(MockResponse(code = 201, body = "{}"))
        t.captureText("x", "A title", "a note", "UTC")
        assertEquals("/api/v1/capture?tz=UTC&title=A%20title&note=a%20note", server.takeRequest().target)
    }

    @Test fun filesGoAsMultipartNamedFile() = runTest {
        server.enqueue(MockResponse(code = 202, body = "{}"))
        val f = kotlin.io.path.createTempFile("cap", ".txt").toFile().apply { writeText("bytes") }
        t.captureFiles(listOf(OutFile(f.path, "note.txt", "text/plain")), null, "n", "UTC")
        val r = server.takeRequest()
        val body = r.body!!.utf8()
        assertTrue(body.contains("name=\"file\"; filename=\"note.txt\""))
        assertTrue(body.contains("name=\"note\""))
        assertTrue(body.contains("bytes"))
    }

    @Test fun a401IsRefused() = runTest {
        server.enqueue(MockResponse(code = 401))
        try { t.momentDone("m1"); fail() } catch (e: Refused) {}
    }

    @Test fun snoozeCarriesUntil() = runTest {
        server.enqueue(MockResponse(code = 204))
        t.momentSnooze("m1", 1_800_000_000L)
        val r = server.takeRequest()
        assertEquals("/api/v1/moments/m1/snooze", r.target)
        assertEquals("""{"until":1800000000}""", r.body!!.utf8())
    }

    @Test fun pushRegistrationIsAPut() = runTest {
        server.enqueue(MockResponse(code = 204))
        t.registerPush("https://push.test/e", "BPxx", "auth")
        val r = server.takeRequest()
        assertEquals("PUT", r.method)
        assertEquals("/api/v1/push/unifiedpush", r.target)
        assertEquals("""{"endpoint":"https://push.test/e","p256dh":"BPxx","auth":"auth"}""", r.body!!.utf8())
    }

    @Test fun vapidReturnsThePublicKey() = runTest {
        server.enqueue(MockResponse(code = 200, body = """{"public_key":"BKEY"}"""))
        assertEquals("BKEY", t.vapid())
    }
}
```

- [ ] **Step 2: Failing tests for Pairing**

```kotlin
package io.github.overcuriousity.engram.core

import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test

class PairingTest {
    private val server = MockWebServer()
    @Before fun up() = server.start()
    @After fun down() = server.close()
    private fun uri(f: String? = null) =
        PairUri(server.url("/").toString().trimEnd('/'), "code123", "0.1.0", f)

    @Test fun aClaimPostsCodeAndDeviceAndKeepsTheAnswer() = runTest {
        server.enqueue(MockResponse(code = 201, body = """{"token":"engram_new","version":"0.1.0"}"""))
        val c = Pairing.claim(uri(), "engram for Android 0.1.0 · Pixel 8", "engram-android/0.1.0 (Pixel 8)")
        assertEquals("engram_new", c.token)
        assertEquals("0.1.0", c.serverVersion)
        assertEquals(uri().origin, c.origin)
        assertNull(c.pin)   // loopback over plain http: nothing to pin
        val r = server.takeRequest()
        assertEquals("/api/v1/pair/claim", r.target)
        assertNull(r.headers["Authorization"])
        assertEquals("engram-android/0.1.0 (Pixel 8)", r.headers["User-Agent"])
        assertEquals("""{"code":"code123","device":"engram for Android 0.1.0 · Pixel 8"}""", r.body!!.utf8())
    }

    @Test fun a401IsClaimRefused() = runTest {
        server.enqueue(MockResponse(code = 401))
        try { Pairing.claim(uri(), "d", "ua"); fail() } catch (e: ClaimRefused) {}
    }

    @Test fun aFingerprintInTheUriBecomesThePin() = runTest {
        // Over plain http the pin is never checked, but it is kept: the app
        // will refuse a later https handshake that does not match it.
        server.enqueue(MockResponse(code = 201, body = """{"token":"t","version":"1"}"""))
        val f = "F".repeat(43)
        assertEquals(f, Pairing.claim(uri(f), "d", "ua").pin)
    }
}
```

- [ ] **Step 3: Run, expect compile failure.**

- [ ] **Step 4: Implement `Transport.kt`**

```kotlin
package io.github.overcuriousity.engram.core

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.CertificatePinner
import okhttp3.HttpUrl.Companion.toHttpUrl
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MultipartBody
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody
import okhttp3.RequestBody.Companion.asRequestBody
import okhttp3.RequestBody.Companion.toRequestBody
import java.io.File
import java.io.IOException
import java.util.concurrent.TimeUnit
import javax.net.ssl.SSLPeerUnverifiedException

/** The server answered 401: paired, and refused. Not retried. */
class Refused : IOException("the server refused the credential")

/** The certificate served is not the one pinned. Not retried, not recoverable. */
class PinMismatch(val expected: String, val served: String) :
    IOException("pinned $expected, served $served")

data class Answer(val status: Int, val body: String)
data class OutFile(val path: String, val name: String, val mime: String)

fun userAgent(versionName: String, model: String) = "engram-android/$versionName ($model)"

internal fun baseClient(userAgent: String, pin: String?, host: String?): OkHttpClient {
    val b = OkHttpClient.Builder()
        .connectTimeout(15, TimeUnit.SECONDS)
        .readTimeout(60, TimeUnit.SECONDS)
        .retryOnConnectionFailure(false)
        .addInterceptor(Interceptor { chain ->
            chain.proceed(chain.request().newBuilder().header("User-Agent", userAgent).build())
        })
    if (pin != null && host != null) {
        b.certificatePinner(CertificatePinner.Builder().add(host, "sha256/$pin").build())
    }
    return b.build()
}

/**
 * The one client. Everything after pairing goes through here, carrying the
 * bearer and the pin; retry is the outbox's and lives nowhere in this file.
 */
internal class Transport(
    val connection: Connection,
    val userAgent: String,
    client: OkHttpClient? = null,
) {
    private val base = connection.origin.toHttpUrl()
    private val client: OkHttpClient = client ?: baseClient(userAgent, connection.pin, base.host)
    private val json = Json { ignoreUnknownKeys = true }

    private fun url(path: String, query: Map<String, String?> = emptyMap()) =
        base.newBuilder().encodedPath(path).apply {
            query.forEach { (k, v) -> if (v != null) addQueryParameter(k, v) }
        }.build()

    private suspend fun send(req: Request): Answer = withContext(Dispatchers.IO) {
        val authed = req.newBuilder().header("Authorization", "Bearer ${connection.token}").build()
        try {
            client.newCall(authed).execute().use { res ->
                if (res.code == 401) throw Refused()
                Answer(res.code, res.body.string())
            }
        } catch (e: SSLPeerUnverifiedException) {
            // OkHttp's message names the pins it saw; the served one is on its
            // second line. Good enough for a screen that only has to be loud.
            throw PinMismatch(connection.pin ?: "", e.message?.lines()?.getOrNull(1)?.trim() ?: "?")
        }
    }

    suspend fun captureText(text: String, title: String?, note: String?, tz: String): Answer =
        send(Request.Builder()
            .url(url("/api/v1/capture", mapOf("tz" to tz, "title" to title, "note" to note)))
            .post(text.toRequestBody("text/plain; charset=utf-8".toMediaType()))
            .build())

    suspend fun captureFiles(files: List<OutFile>, title: String?, note: String?, tz: String): Answer {
        val body = MultipartBody.Builder().setType(MultipartBody.FORM).apply {
            addFormDataPart("tz", tz)
            if (title != null) addFormDataPart("title", title)
            if (note != null) addFormDataPart("note", note)
            files.forEach { f -> addFormDataPart("file", f.name, File(f.path).asRequestBody(f.mime.toMediaType())) }
        }.build()
        return send(Request.Builder().url(url("/api/v1/capture")).post(body).build())
    }

    suspend fun vapid(): String {
        val a = send(Request.Builder().url(url("/api/v1/push/vapid")).get().build())
        if (a.status != 200) throw IOException("vapid: ${a.status}")
        return json.parseToJsonElement(a.body).jsonObject["public_key"]!!.jsonPrimitive.content
    }

    suspend fun registerPush(endpoint: String, p256dh: String, auth: String) {
        val body = """{"endpoint":${q(endpoint)},"p256dh":${q(p256dh)},"auth":${q(auth)}}"""
        val a = send(Request.Builder().url(url("/api/v1/push/unifiedpush")).put(jsonBody(body)).build())
        if (a.status !in 200..299) throw IOException("register push: ${a.status} ${a.body}")
    }

    suspend fun unregisterPush() {
        send(Request.Builder().url(url("/api/v1/push/unifiedpush")).delete().build())
    }

    suspend fun momentDone(id: String) {
        val a = send(Request.Builder().url(url("/api/v1/moments/$id/done")).post(jsonBody("")).build())
        if (a.status !in 200..299 && a.status != 404) throw IOException("done: ${a.status}")
    }

    suspend fun momentSnooze(id: String, until: Long) {
        val a = send(Request.Builder().url(url("/api/v1/moments/$id/snooze"))
            .post(jsonBody("""{"until":$until}""")).build())
        if (a.status !in 200..299 && a.status != 404) throw IOException("snooze: ${a.status}")
    }

    private fun jsonBody(s: String): RequestBody = s.toRequestBody("application/json".toMediaType())
    private fun q(s: String) = Json.encodeToString(kotlinx.serialization.serializer<String>(), s)
}
```

- [ ] **Step 5: Implement `Pairing.kt`**

```kotlin
package io.github.overcuriousity.engram.core

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import java.io.IOException
import java.security.MessageDigest
import java.security.cert.X509Certificate
import java.util.Base64

/** The code was unknown, expired or already used. The server says no more. */
class ClaimRefused : IOException("this code has expired or was already used")

object Pairing {
    /**
     * Trade the scanned code for the real token. The one request made without
     * a bearer. When the QR carried no fingerprint, the leaf certificate the
     * handshake actually served is recorded as the pin — unless the chain was
     * publicly trusted, in which case there is nothing to pin and `pin` stays
     * null. A fingerprint in the QR is the pin from the first byte.
     */
    suspend fun claim(uri: PairUri, deviceName: String, userAgent: String): Connection = withContext(Dispatchers.IO) {
        val client = baseClient(userAgent, uri.fingerprint, java.net.URI(uri.origin).host)
        val body = """{"code":${js(uri.code)},"device":${js(deviceName)}}"""
        val req = Request.Builder()
            .url("${uri.origin}/api/v1/pair/claim")
            .post(body.toRequestBody("application/json".toMediaType()))
            .build()
        client.newCall(req).execute().use { res ->
            if (res.code == 401) throw ClaimRefused()
            if (res.code != 201) throw IOException("claim: ${res.code} ${res.body.string()}")
            val obj = Json.parseToJsonElement(res.body.string()).jsonObject
            val token = obj["token"]!!.jsonPrimitive.content
            val version = obj["version"]?.jsonPrimitive?.content ?: uri.serverVersion
            val pin = uri.fingerprint ?: tofuPin(res.handshake?.peerCertificates?.firstOrNull() as? X509Certificate)
            Connection(uri.origin, token, pin, version, deviceName)
        }
    }

    /** SPKI SHA-256, base64url unpadded — the same string `f=` would carry. */
    internal fun spki(cert: X509Certificate): String =
        Base64.getUrlEncoder().withoutPadding()
            .encodeToString(MessageDigest.getInstance("SHA-256").digest(cert.publicKey.encoded))

    private fun tofuPin(leaf: X509Certificate?): String? {
        leaf ?: return null
        // A chain the system trusts needs no pin: the CA is the guarantee, and
        // a pin would only break the day the operator renews.
        return if (publiclyTrusted(leaf)) null else spki(leaf)
    }

    private fun publiclyTrusted(leaf: X509Certificate): Boolean = runCatching {
        val tmf = javax.net.ssl.TrustManagerFactory.getInstance(javax.net.ssl.TrustManagerFactory.getDefaultAlgorithm())
        tmf.init(null as java.security.KeyStore?)
        val tm = tmf.trustManagers.filterIsInstance<javax.net.ssl.X509TrustManager>().first()
        tm.checkServerTrusted(arrayOf(leaf), "RSA")
        true
    }.getOrDefault(false)

    private fun js(s: String) = Json.encodeToString(kotlinx.serialization.serializer<String>(), s)
}
```

Note: `publiclyTrusted` sees only the leaf, which fails for a chain that needs its intermediate. That errs toward pinning, which is the safe direction; the pin is then the leaf's SPKI and a renewal with a new key will refuse loudly — which the spec accepts.

- [ ] **Step 6: Run both test classes, expect 10 passed; commit**

```bash
git add android/core
git commit -m "feat(android): Transport carries bearer, user agent and pin; Pairing claims a scanned code

Evidence: ./gradlew :core:test --tests '*TransportTest*' --tests '*PairingTest*' — 10 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: `Situation` builds the bundle

**Files:**
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/Situation.kt`
- Test: `android/core/src/test/kotlin/io/github/overcuriousity/engram/core/SituationTest.kt`

**Interfaces:**
- Produces:
  - `interface SituationSource` with one `val`/`fun` per platform reading (below); `class AndroidSituationSource(context: Context) : SituationSource`.
  - `class Situation(private val src: SituationSource, private val stable: Stable)`; `data class Stable(platform, uaFamily, screenW, screenH, cores, memoryGb, language)`; `fun bundle(placeOn: Boolean): JsonObject`.
  - `Stable.of(context)` reads the six stable fields once.

- [ ] **Step 1: Failing tests**

```kotlin
package io.github.overcuriousity.engram.core

import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.jsonPrimitive
import org.junit.Assert.*
import org.junit.Test

class SituationTest {
    private val stable = Stable("Android", "engram-android", 1080, 2400, 8, 8.0, "de-DE")

    private fun src(
        tz: String = "Europe/Berlin", dark: Boolean = true, portrait: Boolean = true,
        battery: Float = 0.5f, charging: Boolean = false, network: String? = "wifi",
        route: String = "speaker", dnd: Boolean = false, place: String? = "u33dc0",
    ) = object : SituationSource {
        override val tz = tz; override val tzOffsetMins = 120
        override val dark = dark; override val portrait = portrait
        override val batteryLevel = battery; override val charging = charging
        override val powerSave = false; override val docked = false
        override val network = network; override val downlinkMbit = 50f; override val saveData = false
        override val audioRoute = route; override val headset = route != "speaker"
        override val dnd = dnd; override val ringer = "normal"
        override val brightness = 0.7f; override val lux = 120f
        override val dpr = 2.75f; override val languages = listOf("de-DE", "en-GB")
        override val hourCycle = "h23"; override val reducedMotion = false; override val highContrast = false
        override val videoInputs = 2; override val audioInputs = 1; override val audioOutputs = 1
        override val sinceLastViewS = 30f; override val viewsToday = 3
        override fun place() = place
    }

    @Test fun theStableHalfDoesNotMoveWithTheSituation() {
        val a = Situation(src(), stable).bundle(placeOn = true)
        val b = Situation(src(tz = "America/New_York", dark = false, portrait = false, battery = 0.1f,
            charging = true, network = "cellular", route = "car", dnd = true, place = "dr5reg"), stable).bundle(placeOn = true)
        for (k in listOf("platform", "ua_family", "screen_w", "screen_h", "cores", "memory_gb", "language")) {
            assertEquals(k, a[k], b[k])
        }
        assertNotEquals(a["audio_route"], b["audio_route"])
    }

    @Test fun everyKeyIsInTheVocabulary() {
        val allowed = javaClass.getResource("/bundle-fields.txt")!!.readText().lines().filter { it.isNotBlank() }.toSet()
        val keys = Situation(src(), stable).bundle(placeOn = true).keys
        assertTrue("outside the vocabulary: ${keys - allowed}", allowed.containsAll(keys))
    }

    @Test fun placeIsAbsentWhenTheSwitchIsOff() {
        assertEquals(JsonNull, Situation(src(), stable).bundle(placeOn = false)["place"])
        assertEquals("u33dc0", Situation(src(), stable).bundle(placeOn = true)["place"]!!.jsonPrimitive.content)
    }

    @Test fun theAppSaysWhatItIs() {
        val b = Situation(src(), stable).bundle(placeOn = false)
        assertEquals("app", b["display_mode"]!!.jsonPrimitive.content)
        assertEquals("coarse", b["pointer"]!!.jsonPrimitive.content)
        assertEquals("true", b["touch"]!!.jsonPrimitive.content)
        assertEquals("portrait", b["orientation"]!!.jsonPrimitive.content)
        assertEquals("dark", b["color_scheme"]!!.jsonPrimitive.content)
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**

- [ ] **Step 3: Implement**

```kotlin
package io.github.overcuriousity.engram.core

import android.app.ActivityManager
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.res.Configuration
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.location.LocationManager
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.BatteryManager
import android.os.PowerManager
import android.app.NotificationManager
import android.provider.Settings
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import kotlinx.serialization.json.putJsonArray
import java.time.ZoneId
import java.time.ZonedDateTime
import java.util.Locale
import kotlin.math.roundToInt

/**
 * The six fields `device_key` on the server hashes. Read once; a phone that
 * rotates, unplugs or moves is the same phone.
 */
data class Stable(
    val platform: String, val uaFamily: String,
    val screenW: Int, val screenH: Int, val cores: Int, val memoryGb: Double, val language: String,
) {
    companion object {
        fun of(context: Context): Stable {
            val wm = context.getSystemService(android.view.WindowManager::class.java)
            val b = wm.maximumWindowMetrics.bounds
            val w = minOf(b.width(), b.height()); val h = maxOf(b.width(), b.height())
            val mi = ActivityManager.MemoryInfo().also { context.getSystemService(ActivityManager::class.java).getMemoryInfo(it) }
            val gb = (mi.totalMem / 1_073_741_824.0 * 2).roundToInt() / 2.0
            return Stable("Android", "engram-android", w, h, Runtime.getRuntime().availableProcessors(), gb,
                Locale.getDefault().toLanguageTag())
        }
    }
}

/** Everything the platform is asked for each time. One property per reading, so a test can fake it. */
interface SituationSource {
    val tz: String; val tzOffsetMins: Int
    val dark: Boolean; val portrait: Boolean
    val batteryLevel: Float?; val charging: Boolean; val powerSave: Boolean; val docked: Boolean
    val network: String?; val downlinkMbit: Float?; val saveData: Boolean
    val audioRoute: String; val headset: Boolean
    val dnd: Boolean; val ringer: String
    val brightness: Float?; val lux: Float?
    val dpr: Float; val languages: List<String>; val hourCycle: String
    val reducedMotion: Boolean; val highContrast: Boolean
    val videoInputs: Int; val audioInputs: Int; val audioOutputs: Int
    val sinceLastViewS: Float?; val viewsToday: Int
    /** 6-character geohash from the last known passive position, or null. Only called when the switch is on. */
    fun place(): String?
}

class Situation(private val src: SituationSource, private val stable: Stable) {
    fun bundle(placeOn: Boolean): JsonObject = buildJsonObject {
        // The nineteen the browser has always sent.
        put("tz", src.tz); put("tz_offset_mins", src.tzOffsetMins)
        put("language", stable.language); putJsonArray("languages") { src.languages.forEach { add(JsonPrimitive(it)) } }
        put("viewport_w", stable.screenW); put("viewport_h", stable.screenH)
        put("screen_w", stable.screenW); put("screen_h", stable.screenH)
        put("dpr", src.dpr); put("color_scheme", if (src.dark) "dark" else "light")
        put("platform", stable.platform); put("ua_family", stable.uaFamily)
        put("cores", stable.cores); put("memory_gb", stable.memoryGb); put("touch", true)
        put("orientation", if (src.portrait) "portrait" else "landscape")
        put("network", src.network); put("battery_level", src.batteryLevel); put("charging", src.charging)
        put("audio_outputs", src.audioOutputs)
        // Stored, not encoded — the wider vocabulary.
        put("place", if (placeOn) src.place() else null)
        put("net_effective", null as String?); put("net_downlink", src.downlinkMbit); put("rtt", null as Float?)
        put("save_data", src.saveData); put("reduced_motion", src.reducedMotion); put("high_contrast", src.highContrast)
        put("pointer", "coarse"); put("hover", false); put("display_mode", "app")
        put("nav_type", null as String?); put("window_state", null as String?); put("focused", true)
        put("since_last_view_s", src.sinceLastViewS); put("views_today", src.viewsToday)
        put("screen_x", null as Float?); put("screen_y", null as Float?); put("screens", 1)
        put("avail_w", null as Float?); put("avail_h", null as Float?)
        put("video_inputs", src.videoInputs); put("audio_inputs", src.audioInputs)
        put("zoom", null as Float?); put("fullscreen", false); put("referrer_kind", null as String?)
        put("online", src.network != null); put("keyboard_layout", null as String?); put("hour_cycle", src.hourCycle)
        put("audio_route", src.audioRoute); put("dnd", src.dnd); put("ringer", src.ringer)
        put("power_save", src.powerSave); put("brightness", src.brightness); put("lux", src.lux)
        put("docked", src.docked); put("headset", src.headset)
    }
}

/** The real readings. Every one is a getter, so a bundle is the moment it is built. */
class AndroidSituationSource(private val context: Context, private val counters: ViewCounters) : SituationSource {
    private val audio get() = context.getSystemService(AudioManager::class.java)
    private val conn get() = context.getSystemService(ConnectivityManager::class.java)
    private val power get() = context.getSystemService(PowerManager::class.java)
    private val notif get() = context.getSystemService(NotificationManager::class.java)
    private val battery get() = context.registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
    private val caps get() = conn.activeNetwork?.let { conn.getNetworkCapabilities(it) }

    override val tz get() = ZoneId.systemDefault().id
    override val tzOffsetMins get() = ZonedDateTime.now().offset.totalSeconds / 60
    override val dark get() = context.resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK == Configuration.UI_MODE_NIGHT_YES
    override val portrait get() = context.resources.configuration.orientation != Configuration.ORIENTATION_LANDSCAPE
    override val batteryLevel: Float? get() = battery?.let {
        val l = it.getIntExtra(BatteryManager.EXTRA_LEVEL, -1); val s = it.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
        if (l < 0 || s <= 0) null else l.toFloat() / s
    }
    override val charging get() = (battery?.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) ?: 0) != 0
    override val powerSave get() = power.isPowerSaveMode
    override val docked: Boolean get() {
        val plugged = battery?.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) ?: 0
        val dock = context.registerReceiver(null, IntentFilter(Intent.ACTION_DOCK_EVENT))
            ?.getIntExtra(Intent.EXTRA_DOCK_STATE, Intent.EXTRA_DOCK_STATE_UNDOCKED) ?: Intent.EXTRA_DOCK_STATE_UNDOCKED
        return plugged == BatteryManager.BATTERY_PLUGGED_WIRELESS || dock != Intent.EXTRA_DOCK_STATE_UNDOCKED
    }
    override val network: String? get() = caps?.let {
        when {
            it.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> "wifi"
            it.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> "cellular"
            it.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> "wired"
            else -> "other"
        }
    }
    override val downlinkMbit: Float? get() = caps?.linkDownstreamBandwidthKbps?.takeIf { it > 0 }?.let { it / 1000f }
    override val saveData get() = conn.isActiveNetworkMetered && conn.restrictBackgroundStatus == ConnectivityManager.RESTRICT_BACKGROUND_STATUS_ENABLED
    override val audioRoute: String get() {
        val outs = audio.getDevices(AudioManager.GET_DEVICES_OUTPUTS)
        return when {
            outs.any { it.type == AudioDeviceInfo.TYPE_BLUETOOTH_A2DP && it.productName.contains("car", true) } -> "car"
            outs.any { it.type == AudioDeviceInfo.TYPE_BLUETOOTH_A2DP || it.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO } -> "bluetooth"
            outs.any { it.type == AudioDeviceInfo.TYPE_WIRED_HEADPHONES || it.type == AudioDeviceInfo.TYPE_WIRED_HEADSET || it.type == AudioDeviceInfo.TYPE_USB_HEADSET } -> "wired"
            else -> "speaker"
        }
    }
    override val headset get() = audioRoute == "wired" || audioRoute == "bluetooth"
    override val dnd get() = notif.currentInterruptionFilter != NotificationManager.INTERRUPTION_FILTER_ALL
    override val ringer get() = when (audio.ringerMode) {
        AudioManager.RINGER_MODE_SILENT -> "silent"; AudioManager.RINGER_MODE_VIBRATE -> "vibrate"; else -> "normal"
    }
    override val brightness: Float? get() = runCatching {
        Settings.System.getInt(context.contentResolver, Settings.System.SCREEN_BRIGHTNESS) / 255f
    }.getOrNull()
    override val lux: Float? get() = LightSample.last
    override val dpr get() = context.resources.displayMetrics.density
    override val languages: List<String> get() {
        val l = context.resources.configuration.locales
        return (0 until l.size()).map { l[it].toLanguageTag() }
    }
    override val hourCycle get() = if (android.text.format.DateFormat.is24HourFormat(context)) "h23" else "h12"
    override val reducedMotion get() = Settings.Global.getFloat(context.contentResolver, Settings.Global.ANIMATOR_DURATION_SCALE, 1f) == 0f
    override val highContrast get() = runCatching { Settings.Secure.getInt(context.contentResolver, "high_text_contrast_enabled") == 1 }.getOrDefault(false)
    override val videoInputs get() = runCatching { context.getSystemService(android.hardware.camera2.CameraManager::class.java).cameraIdList.size }.getOrDefault(0)
    override val audioInputs get() = audio.getDevices(AudioManager.GET_DEVICES_INPUTS).size
    override val audioOutputs get() = audio.getDevices(AudioManager.GET_DEVICES_OUTPUTS).size
    override val sinceLastViewS get() = counters.sinceLastViewS()
    override val viewsToday get() = counters.viewsToday()

    override fun place(): String? {
        val lm = context.getSystemService(LocationManager::class.java)
        val loc = runCatching { lm.getLastKnownLocation(LocationManager.PASSIVE_PROVIDER) }.getOrNull() ?: return null
        return Geohash.encode(loc.latitude, loc.longitude, 6)
    }
}

/** The light sensor's last reading, kept by whoever registers it (the Activity while resumed). */
object LightSample : SensorEventListener {
    @Volatile var last: Float? = null
    fun start(context: Context) {
        val sm = context.getSystemService(SensorManager::class.java)
        sm.getDefaultSensor(Sensor.TYPE_LIGHT)?.let { sm.registerListener(this, it, SensorManager.SENSOR_DELAY_NORMAL) }
    }
    fun stop(context: Context) = context.getSystemService(SensorManager::class.java).unregisterListener(this)
    override fun onSensorChanged(e: SensorEvent) { last = e.values.firstOrNull() }
    override fun onAccuracyChanged(s: Sensor?, a: Int) {}
}

/** Same two counters the browser keeps in localStorage. */
class ViewCounters(private val prefs: android.content.SharedPreferences) {
    fun sinceLastViewS(): Float? = prefs.getLong("last_view", 0L).takeIf { it > 0 }?.let { (System.currentTimeMillis() - it) / 1000f }
    fun viewsToday(): Int {
        val day = java.time.LocalDate.now().toString()
        val n = if (prefs.getString("views_day", "") == day) prefs.getInt("views_n", 0) else 0
        return n + 1
    }
    fun mark() {
        val day = java.time.LocalDate.now().toString()
        prefs.edit().putLong("last_view", System.currentTimeMillis()).putString("views_day", day).putInt("views_n", viewsToday()).apply()
    }
}

/** Standard geohash, the same function as `geohash()` in app.js. */
object Geohash {
    private const val CHARS = "0123456789bcdefghjkmnpqrstuvwxyz"
    fun encode(lat: Double, lon: Double, precision: Int): String {
        var latR = doubleArrayOf(-90.0, 90.0); var lonR = doubleArrayOf(-180.0, 180.0)
        val out = StringBuilder(); var bit = 0; var ch = 0; var even = true
        while (out.length < precision) {
            if (even) { val mid = (lonR[0] + lonR[1]) / 2; if (lon >= mid) { ch = (ch shl 1) or 1; lonR[0] = mid } else { ch = ch shl 1; lonR[1] = mid } }
            else { val mid = (latR[0] + latR[1]) / 2; if (lat >= mid) { ch = (ch shl 1) or 1; latR[0] = mid } else { ch = ch shl 1; latR[1] = mid } }
            even = !even
            if (++bit == 5) { out.append(CHARS[ch]); bit = 0; ch = 0 }
        }
        return out.toString()
    }
}
```

Add one more test to `SituationTest`: `Geohash.encode(52.52, 13.405, 6) == "u33dc0"` — Berlin, the same cell the browser test in the vocabulary plan uses. `AndroidSituationSource` is not unit-tested (it is all platform reads); it is exercised by the Settings screen on a device.

- [ ] **Step 4: Run, expect 5 passed; commit**

```bash
git add android/core
git commit -m "feat(android): Situation builds the bundle, stable half fixed, place behind the switch

Evidence: ./gradlew :core:test --tests '*SituationTest*' — 5 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: The outbox in Room, with its transitions and the backoff

**Files:**
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/db/Db.kt`
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/outbox/Backoff.kt`
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/outbox/Outbox.kt`
- Test: `android/core/src/test/kotlin/io/github/overcuriousity/engram/core/outbox/BackoffTest.kt`, `OutboxTest.kt`

**Interfaces:**
- Produces:
  - `@Database class Db : RoomDatabase` with `outboxDao()` and `momentsDao()`; `Db.open(context)` for the app, `Db.inMemory()` for tests (bundled driver, no Android).
  - Entities `OutboxRow(id, kind, payload, createdAt, attempts, nextAt, state, status, answer, error)`, `OutboxFile(outboxId, path, name, mime)`, `MomentRow(id, title, at, fetchedAt)`.
  - `enum class Kind { capture_text, capture_files, done, snooze }`, `enum class State { queued, sent, refused, held }`.
  - `object Backoff { fun delayMs(attempts: Int): Long }`.
  - `class Outbox(db: Db, dir: File, clock: () -> Long = System::currentTimeMillis)` with
    - `suspend fun enqueueText(text: String, title: String?, note: String?): String`
    - `suspend fun enqueueFiles(files: List<Incoming>, title: String?, note: String?): String` where `class Incoming(val name: String, val mime: String, val open: () -> InputStream)`
    - `suspend fun enqueueDone(momentId: String): String`; `suspend fun enqueueSnooze(momentId: String, until: Long): String`
    - `suspend fun patchNote(id: String, note: String): Boolean` (only while `queued`)
    - `val rows: Flow<List<OutboxRow>>`; `suspend fun filesOf(id): List<OutboxFile>`
    - `suspend fun sent(id, status, body)`, `suspend fun failed(id, error)`, `suspend fun held(id, status, body)`, `suspend fun refuseAll()`, `suspend fun requeueRefused()`, `suspend fun delete(id)`, `suspend fun sweepSent(olderThanMs: Long)`
    - `suspend fun dueQueued(now: Long): List<OutboxRow>`
  - Payload JSON shapes: text `{"text","title","note"}`; files `{"title","note"}`; done `{"moment"}`; snooze `{"moment","until"}`.

- [ ] **Step 1: Backoff test and implementation**

```kotlin
package io.github.overcuriousity.engram.core.outbox

import org.junit.Assert.assertEquals
import org.junit.Test

class BackoffTest {
    @Test fun theScheduleIsTheSpecs() {
        assertEquals(30_000L, Backoff.delayMs(1))
        assertEquals(120_000L, Backoff.delayMs(2))
        assertEquals(600_000L, Backoff.delayMs(3))
        assertEquals(1_800_000L, Backoff.delayMs(4))
        assertEquals(3_600_000L, Backoff.delayMs(5))
        assertEquals(7_200_000L, Backoff.delayMs(6))
        assertEquals(7_200_000L, Backoff.delayMs(60))
    }
}
```

```kotlin
package io.github.overcuriousity.engram.core.outbox

/** After the n-th failed attempt, wait this long. Ends flat at two hours: a row is never given up on. */
object Backoff {
    private val LADDER = longArrayOf(30_000, 120_000, 600_000, 1_800_000, 3_600_000, 7_200_000)
    fun delayMs(attempts: Int): Long = LADDER[(attempts - 1).coerceIn(0, LADDER.size - 1)]
}
```

- [ ] **Step 2: Failing Outbox tests**

```kotlin
package io.github.overcuriousity.engram.core.outbox

import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.State
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test
import java.io.File
import kotlin.io.path.createTempDirectory

class OutboxTest {
    private var now = 1_000_000L
    private val dir = createTempDirectory("outbox").toFile()
    private val db = Db.inMemory()
    private val box = Outbox(db, dir) { now }

    @Test fun aTextCaptureIsQueuedAtOnce() = runTest {
        val id = box.enqueueText("hello", null, "n")
        val row = box.rows.first().single()
        assertEquals(id, row.id)
        assertEquals(Kind.capture_text, row.kind)
        assertEquals(State.queued, row.state)
        assertEquals(now, row.nextAt)
        assertTrue(row.payload.contains("\"text\":\"hello\""))
    }

    @Test fun filesAreCopiedBeforeTheRowExists() = runTest {
        val id = box.enqueueFiles(listOf(Incoming("a.txt", "text/plain") { "abc".byteInputStream() }), "t", null)
        val f = box.filesOf(id).single()
        assertEquals("a.txt", f.name)
        assertEquals("abc", File(f.path).readText())
        assertTrue(f.path.startsWith(File(dir, id).path))
    }

    @Test fun aCopyThatFailsLeavesNoRow() = runTest {
        try {
            box.enqueueFiles(listOf(Incoming("x", "text/plain") { throw java.io.IOException("gone") }), null, null)
            fail()
        } catch (e: java.io.IOException) {}
        assertTrue(box.rows.first().isEmpty())
        assertTrue(dir.listFiles().isNullOrEmpty())
    }

    @Test fun sentKeepsTheAnswerAndDropsTheFiles() = runTest {
        val id = box.enqueueFiles(listOf(Incoming("a", "text/plain") { "a".byteInputStream() }), null, null)
        box.sent(id, 201, """{"id":"art"}""")
        val row = box.rows.first().single()
        assertEquals(State.sent, row.state); assertEquals(201, row.status); assertEquals("""{"id":"art"}""", row.answer)
        assertFalse(File(dir, id).exists())
    }

    @Test fun aFailureMovesNextAtOutOnTheLadder() = runTest {
        val id = box.enqueueText("x", null, null)
        box.failed(id, "timeout")
        val r1 = box.rows.first().single()
        assertEquals(1, r1.attempts); assertEquals(now + 30_000, r1.nextAt); assertEquals("timeout", r1.error)
        box.failed(id, "timeout")
        assertEquals(now + 120_000, box.rows.first().single().nextAt)
        assertTrue(box.dueQueued(now).isEmpty())
        assertEquals(1, box.dueQueued(now + 120_000).size)
    }

    @Test fun heldKeepsTheFilesAndTheServersWords() = runTest {
        val id = box.enqueueFiles(listOf(Incoming("a", "text/plain") { "a".byteInputStream() }), null, null)
        box.held(id, 400, "that body is not valid UTF-8 text")
        val row = box.rows.first().single()
        assertEquals(State.held, row.state); assertEquals(400, row.status)
        assertEquals("that body is not valid UTF-8 text", row.error)
        assertTrue(File(dir, id).exists())
    }

    @Test fun refuseAllAndRequeue() = runTest {
        box.enqueueText("a", null, null); box.enqueueText("b", null, null)
        val s = box.enqueueText("c", null, null); box.sent(s, 201, "{}")
        box.refuseAll()
        val states = box.rows.first().map { it.state }
        assertEquals(2, states.count { it == State.refused }); assertEquals(1, states.count { it == State.sent })
        box.requeueRefused()
        assertEquals(2, box.dueQueued(now).size)
    }

    @Test fun aNoteIsPatchedOnlyWhileQueued() = runTest {
        val id = box.enqueueText("a", null, null)
        assertTrue(box.patchNote(id, "later"))
        assertTrue(box.rows.first().single().payload.contains("\"note\":\"later\""))
        box.sent(id, 201, "{}")
        assertFalse(box.patchNote(id, "too late"))
    }

    @Test fun sentRowsAreSweptAfterTheirTime() = runTest {
        val id = box.enqueueText("a", null, null); box.sent(id, 201, "{}")
        box.sweepSent(olderThanMs = 7L * 24 * 3600 * 1000)
        assertEquals(1, box.rows.first().size)
        now += 8L * 24 * 3600 * 1000
        box.sweepSent(olderThanMs = 7L * 24 * 3600 * 1000)
        assertTrue(box.rows.first().isEmpty())
    }

    @Test fun doneAndSnoozeAreRows() = runTest {
        box.enqueueDone("m1"); box.enqueueSnooze("m2", 1_800_000_000L)
        val rows = box.rows.first().sortedBy { it.kind.name }
        assertEquals(Kind.done, rows[0].kind); assertTrue(rows[0].payload.contains("\"moment\":\"m1\""))
        assertEquals(Kind.snooze, rows[1].kind); assertTrue(rows[1].payload.contains("\"until\":1800000000"))
    }
}
```

- [ ] **Step 3: Run, expect compile failure.**

- [ ] **Step 4: Implement `db/Db.kt`**

```kotlin
package io.github.overcuriousity.engram.core.db

import android.content.Context
import androidx.room.*
import androidx.sqlite.driver.bundled.BundledSQLiteDriver
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.Flow

enum class Kind { capture_text, capture_files, done, snooze }
enum class State { queued, sent, refused, held }

/** Every write the device owes the server. Authoritative; the screens draw it. */
@Entity(tableName = "outbox")
data class OutboxRow(
    @PrimaryKey val id: String,
    val kind: Kind,
    val payload: String,
    val createdAt: Long,
    val attempts: Int = 0,
    val nextAt: Long,
    val state: State = State.queued,
    val status: Int? = null,
    val answer: String? = null,
    val error: String? = null,
)

@Entity(tableName = "outbox_files", primaryKeys = ["outboxId", "path"])
data class OutboxFile(val outboxId: String, val path: String, val name: String, val mime: String)

/** What a push payload said was due. Read by the notification and Settings; Part E grows this. */
@Entity(tableName = "moments")
data class MomentRow(@PrimaryKey val id: String, val title: String, val at: Long, val fetchedAt: Long)

@Dao
interface OutboxDao {
    @Insert suspend fun insert(row: OutboxRow)
    @Insert suspend fun insertFiles(files: List<OutboxFile>)
    @Update suspend fun update(row: OutboxRow)
    @Query("SELECT * FROM outbox WHERE id = :id") suspend fun get(id: String): OutboxRow?
    @Query("SELECT * FROM outbox ORDER BY createdAt DESC") fun all(): Flow<List<OutboxRow>>
    @Query("SELECT * FROM outbox WHERE state = 'queued' AND nextAt <= :now ORDER BY createdAt ASC")
    suspend fun dueQueued(now: Long): List<OutboxRow>
    @Query("SELECT * FROM outbox_files WHERE outboxId = :id") suspend fun filesOf(id: String): List<OutboxFile>
    @Query("UPDATE outbox SET state = 'refused' WHERE state = 'queued'") suspend fun refuseAll()
    @Query("UPDATE outbox SET state = 'queued', nextAt = :now WHERE state = 'refused'") suspend fun requeueRefused(now: Long)
    @Query("DELETE FROM outbox WHERE id = :id") suspend fun delete(id: String)
    @Query("DELETE FROM outbox_files WHERE outboxId = :id") suspend fun deleteFiles(id: String)
    @Query("SELECT id FROM outbox WHERE state = 'sent' AND createdAt < :before") suspend fun sentBefore(before: Long): List<String>
}

@Dao
interface MomentsDao {
    @Upsert suspend fun upsert(rows: List<MomentRow>)
    @Query("SELECT * FROM moments ORDER BY at DESC LIMIT 1") fun latest(): Flow<MomentRow?>
}

@Database(entities = [OutboxRow::class, OutboxFile::class, MomentRow::class], version = 1, exportSchema = true)
abstract class Db : RoomDatabase() {
    abstract fun outboxDao(): OutboxDao
    abstract fun momentsDao(): MomentsDao

    companion object {
        fun open(context: Context): Db =
            Room.databaseBuilder(context, Db::class.java, "engram.db")
                .setDriver(BundledSQLiteDriver())
                .setQueryCoroutineContext(Dispatchers.IO)
                .build()

        /** JVM tests: the bundled driver needs no Android. */
        fun inMemory(): Db =
            Room.inMemoryDatabaseBuilder<Db>()   // KMP-style builder, no Context
                .setDriver(BundledSQLiteDriver())
                .setQueryCoroutineContext(Dispatchers.IO)
                .build()
    }
}
```

If `Room.inMemoryDatabaseBuilder<Db>()` without a `Context` does not exist in this Room version, use `Room.inMemoryDatabaseBuilder(name = ":memory:", factory = { Db_Impl() })` per the Room KMP docs, and note it in the commit. The `enum` columns need `@TypeConverters`; add:

```kotlin
class Converters {
    @TypeConverter fun kindTo(k: Kind) = k.name
    @TypeConverter fun kindFrom(s: String) = Kind.valueOf(s)
    @TypeConverter fun stateTo(s: State) = s.name
    @TypeConverter fun stateFrom(s: String) = State.valueOf(s)
}
```

and `@TypeConverters(Converters::class)` on `Db`.

- [ ] **Step 5: Implement `outbox/Outbox.kt`**

```kotlin
package io.github.overcuriousity.engram.core.outbox

import io.github.overcuriousity.engram.core.db.*
import kotlinx.coroutines.flow.Flow
import kotlinx.serialization.json.*
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.util.UUID

class Incoming(val name: String, val mime: String, val open: () -> InputStream)

/**
 * The load-bearing idea. A share is copied out of the sender's URI into our
 * own storage and written as a row before anything touches the network; the
 * caller is answered at once, and a worker owes the server the rest.
 */
class Outbox(private val db: Db, private val dir: File, private val clock: () -> Long = System::currentTimeMillis) {
    private val dao = db.outboxDao()
    val rows: Flow<List<OutboxRow>> = dao.all()

    suspend fun enqueueText(text: String, title: String?, note: String?): String =
        insert(Kind.capture_text, buildJsonObject { put("text", text); put("title", title); put("note", note) })

    suspend fun enqueueFiles(files: List<Incoming>, title: String?, note: String?): String {
        val id = UUID.randomUUID().toString()
        val folder = File(dir, id)
        val copied = try {
            folder.mkdirs()
            files.mapIndexed { i, f ->
                val safe = f.name.replace(Regex("[^A-Za-z0-9._-]"), "_").ifEmpty { "file" }
                val target = File(folder, "$i-$safe")
                f.open().use { input -> target.outputStream().use { input.copyTo(it) } }
                OutboxFile(id, target.path, f.name, f.mime)
            }
        } catch (e: IOException) {
            folder.deleteRecursively()
            throw e
        }
        val now = clock()
        dao.insert(OutboxRow(id, Kind.capture_files, buildJsonObject { put("title", title); put("note", note) }.toString(), now, nextAt = now))
        dao.insertFiles(copied)
        return id
    }

    suspend fun enqueueDone(momentId: String) = insert(Kind.done, buildJsonObject { put("moment", momentId) })
    suspend fun enqueueSnooze(momentId: String, until: Long) =
        insert(Kind.snooze, buildJsonObject { put("moment", momentId); put("until", until) })

    private suspend fun insert(kind: Kind, payload: JsonObject): String {
        val id = UUID.randomUUID().toString()
        val now = clock()
        dao.insert(OutboxRow(id, kind, payload.toString(), now, nextAt = now))
        return id
    }

    suspend fun patchNote(id: String, note: String): Boolean {
        val row = dao.get(id) ?: return false
        if (row.state != State.queued) return false
        val p = Json.parseToJsonElement(row.payload).jsonObject.toMutableMap()
        p["note"] = JsonPrimitive(note)
        dao.update(row.copy(payload = JsonObject(p).toString()))
        return true
    }

    suspend fun filesOf(id: String) = dao.filesOf(id)
    suspend fun dueQueued(now: Long) = dao.dueQueued(now)

    suspend fun sent(id: String, status: Int, body: String) {
        val row = dao.get(id) ?: return
        dao.update(row.copy(state = State.sent, status = status, answer = body, error = null))
        dropFiles(id)
    }

    suspend fun failed(id: String, error: String) {
        val row = dao.get(id) ?: return
        val attempts = row.attempts + 1
        dao.update(row.copy(attempts = attempts, nextAt = clock() + Backoff.delayMs(attempts), error = error))
    }

    /** A 4xx that is not 401: the server refused this row for what it is. Files stay, so the person can see what. */
    suspend fun held(id: String, status: Int, body: String) {
        val row = dao.get(id) ?: return
        dao.update(row.copy(state = State.held, status = status, answer = body, error = body))
    }

    suspend fun refuseAll() = dao.refuseAll()
    suspend fun requeueRefused() = dao.requeueRefused(clock())

    suspend fun delete(id: String) { dao.delete(id); dropFiles(id) }

    suspend fun sweepSent(olderThanMs: Long) {
        dao.sentBefore(clock() - olderThanMs).forEach { delete(it) }
    }

    private suspend fun dropFiles(id: String) {
        dao.deleteFiles(id)
        File(dir, id).deleteRecursively()
    }
}
```

- [ ] **Step 6: Run, expect 11 passed (1 backoff + 10 outbox); commit**

```bash
git add android/core
git commit -m "feat(android): the outbox — Room rows, files copied first, the four states and the ladder

Evidence: ./gradlew :core:test --tests '*outbox*' — 11 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 7: `Drainer` delivers, `SyncWorker` schedules

**Files:**
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/outbox/Drainer.kt`
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/sync/SyncWorker.kt`
- Test: `android/core/src/test/kotlin/io/github/overcuriousity/engram/core/outbox/DrainerTest.kt`

**Interfaces:**
- Produces:
  - `internal class Drainer(outbox: Outbox, transport: Transport, tz: () -> String, clock: () -> Long)` with `suspend fun drainOnce(): Outcome`; `sealed class Outcome { object Done; data class Later(val nextAt: Long); object Refused; data class Pinned(val e: PinMismatch) }`.
  - `class SyncWorker(ctx, params) : CoroutineWorker`; `object Sync { fun kick(context); fun scheduleAt(context, atMs); fun cancel(context) }` with unique work name `"engram-sync"`.
  - `Engram.refused: MutableStateFlow<Boolean>` and `Engram.pinMismatch: MutableStateFlow<PinMismatch?>` are set by the worker (the façade is Task 8's file; here the worker takes them via `Engram.get(context)`).

- [ ] **Step 1: Failing tests**

```kotlin
package io.github.overcuriousity.engram.core.outbox

import io.github.overcuriousity.engram.core.Connection
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.State
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.SocketEffect
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import kotlin.io.path.createTempDirectory

class DrainerTest {
    private val server = MockWebServer()
    private var now = 5_000_000L
    private val db = Db.inMemory()
    private val box = Outbox(db, createTempDirectory("d").toFile()) { now }
    private lateinit var drainer: Drainer

    @Before fun up() {
        server.start()
        val t = Transport(Connection(server.url("/").toString().trimEnd('/'), "tok", null, "1", "d"), "ua")
        drainer = Drainer(box, t, { "Europe/Berlin" }) { now }
    }
    @After fun down() = server.close()

    @Test fun aQueuedTextIsSentAndItsAnswerKept() = runTest {
        server.enqueue(MockResponse(code = 202, body = """{"status":"reading"}"""))
        box.enqueueText("hi", null, null)
        assertEquals(Drainer.Outcome.Done, drainer.drainOnce())
        val r = box.rows.first().single()
        assertEquals(State.sent, r.state); assertEquals(202, r.status); assertEquals("""{"status":"reading"}""", r.answer)
        assertEquals("/api/v1/capture?tz=Europe%2FBerlin", server.takeRequest().target)
    }

    @Test fun rowsGoInOrderAndAFailureMovesOneOut() = runTest {
        box.enqueueText("first", null, null); now += 1; box.enqueueText("second", null, null)
        server.enqueue(MockResponse.Builder().onRequestStart(SocketEffect.CloseSocket()).build())
        server.enqueue(MockResponse(code = 201, body = "{}"))
        val out = drainer.drainOnce()
        assertTrue(out is Drainer.Outcome.Later)
        assertEquals(now + 30_000, (out as Drainer.Outcome.Later).nextAt)
        val rows = box.rows.first().sortedBy { it.createdAt }
        assertEquals(State.queued, rows[0].state); assertEquals(1, rows[0].attempts)
        assertEquals(State.sent, rows[1].state)
    }

    @Test fun a400IsHeldAndTheRestContinue() = runTest {
        box.enqueueText("bad", null, null); now += 1; box.enqueueText("good", null, null)
        server.enqueue(MockResponse(code = 400, body = """{"error":"that body is not valid UTF-8 text"}"""))
        server.enqueue(MockResponse(code = 201, body = "{}"))
        assertEquals(Drainer.Outcome.Done, drainer.drainOnce())
        val rows = box.rows.first().sortedBy { it.createdAt }
        assertEquals(State.held, rows[0].state); assertEquals(State.sent, rows[1].state)
    }

    @Test fun a401RefusesEverythingAndStops() = runTest {
        box.enqueueText("a", null, null); now += 1; box.enqueueText("b", null, null)
        server.enqueue(MockResponse(code = 401))
        assertEquals(Drainer.Outcome.Refused, drainer.drainOnce())
        assertTrue(box.rows.first().all { it.state == State.refused })
        assertEquals(1, server.requestCount)
    }

    @Test fun a5xxIsRetriedLikeANetworkFailure() = runTest {
        box.enqueueText("a", null, null)
        server.enqueue(MockResponse(code = 503))
        assertTrue(drainer.drainOnce() is Drainer.Outcome.Later)
        assertEquals(State.queued, box.rows.first().single().state)
    }

    @Test fun doneAndSnoozeAreDelivered() = runTest {
        box.enqueueDone("m1"); now += 1; box.enqueueSnooze("m2", 1_800_000_000L)
        server.enqueue(MockResponse(code = 204)); server.enqueue(MockResponse(code = 204))
        assertEquals(Drainer.Outcome.Done, drainer.drainOnce())
        assertEquals("/api/v1/moments/m1/done", server.takeRequest().target)
        assertEquals("/api/v1/moments/m2/snooze", server.takeRequest().target)
    }

    @Test fun nothingDueIsDone() = runTest {
        box.enqueueText("a", null, null); box.failed(box.rows.first().single().id, "x")
        val out = drainer.drainOnce()
        assertTrue(out is Drainer.Outcome.Later)
        assertEquals(0, server.requestCount)
    }
}
```

- [ ] **Step 2: Run, expect compile failure.**

- [ ] **Step 3: Implement `Drainer.kt`**

```kotlin
package io.github.overcuriousity.engram.core.outbox

import io.github.overcuriousity.engram.core.OutFile
import io.github.overcuriousity.engram.core.PinMismatch
import io.github.overcuriousity.engram.core.Refused
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.OutboxRow
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import java.io.IOException

/**
 * One pass over what is due, in the order it was owed. One attempt per row per
 * pass; the ladder decides when the next pass is worth making.
 */
internal class Drainer(
    private val outbox: Outbox,
    private val transport: Transport,
    private val tz: () -> String,
    private val clock: () -> Long,
) {
    sealed class Outcome {
        object Done : Outcome()
        data class Later(val nextAt: Long) : Outcome()
        object Refused : Outcome()
        data class Pinned(val e: PinMismatch) : Outcome()
    }

    suspend fun drainOnce(): Outcome {
        var failed = false
        for (row in outbox.dueQueued(clock())) {
            try {
                deliver(row)
            } catch (e: Refused) {
                outbox.refuseAll()
                return Outcome.Refused
            } catch (e: PinMismatch) {
                return Outcome.Pinned(e)
            } catch (e: IOException) {
                outbox.failed(row.id, e.message ?: e.javaClass.simpleName)
                failed = true
            }
        }
        val pending = outbox.dueQueued(Long.MAX_VALUE).minOfOrNull { it.nextAt }
        return if (pending == null) Outcome.Done else Outcome.Later(pending)
    }

    private suspend fun deliver(row: OutboxRow) {
        val p = Json.parseToJsonElement(row.payload).jsonObject
        fun s(k: String) = p[k]?.jsonPrimitive?.takeIf { it !is kotlinx.serialization.json.JsonNull }?.content
        when (row.kind) {
            Kind.capture_text -> settle(row, transport.captureText(s("text") ?: "", s("title"), s("note"), tz()))
            Kind.capture_files -> {
                val files = outbox.filesOf(row.id).map { OutFile(it.path, it.name, it.mime) }
                settle(row, transport.captureFiles(files, s("title"), s("note"), tz()))
            }
            Kind.done -> { transport.momentDone(s("moment")!!); outbox.sent(row.id, 204, "") }
            Kind.snooze -> {
                transport.momentSnooze(s("moment")!!, p["until"]!!.jsonPrimitive.longOrNull ?: 0L)
                outbox.sent(row.id, 204, "")
            }
        }
    }

    private suspend fun settle(row: OutboxRow, a: io.github.overcuriousity.engram.core.Answer) {
        when (a.status) {
            in 200..299 -> outbox.sent(row.id, a.status, a.body)
            in 400..499 -> outbox.held(row.id, a.status, message(a.body))
            else -> throw IOException("server answered ${a.status}")
        }
    }

    /** The server's `{"error": "..."}` if that is what came back, else the body. */
    private fun message(body: String): String =
        runCatching { Json.parseToJsonElement(body).jsonObject["error"]?.jsonPrimitive?.content }.getOrNull() ?: body
}
```

The outcome is decided by `pending` alone: `Done` when no queued row remains, otherwise `Later(min nextAt)`. `failed` is kept only so a reader sees that a failure does not stop the pass; drop the variable if the linter objects.

- [ ] **Step 4: Implement `sync/SyncWorker.kt`**

```kotlin
package io.github.overcuriousity.engram.core.sync

import android.content.Context
import androidx.work.*
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.outbox.Drainer
import java.util.concurrent.TimeUnit

/** Drains the outbox. Unique, so two never run at once; rescheduled at the nearest rung after each pass. */
class SyncWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result {
        val engram = Engram.get(applicationContext)
        val drainer = engram.drainer() ?: return Result.success()   // unpaired: nothing owed to anyone
        engram.outbox.sweepSent(olderThanMs = 7L * 24 * 3600 * 1000)
        return when (val out = drainer.drainOnce()) {
            Drainer.Outcome.Done -> Result.success()
            is Drainer.Outcome.Later -> { Sync.scheduleAt(applicationContext, out.nextAt); Result.success() }
            Drainer.Outcome.Refused -> { engram.refused.value = true; Result.success() }
            is Drainer.Outcome.Pinned -> { engram.pinMismatch.value = out.e; Result.success() }
        }
    }
}

object Sync {
    private const val NAME = "engram-sync"
    private val online = Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build()

    /** Something new is owed: run as soon as there is a network. */
    fun kick(context: Context) {
        WorkManager.getInstance(context).enqueueUniqueWork(
            NAME, ExistingWorkPolicy.KEEP,
            OneTimeWorkRequestBuilder<SyncWorker>().setConstraints(online).build(),
        )
    }

    fun scheduleAt(context: Context, atMs: Long) {
        val delay = (atMs - System.currentTimeMillis()).coerceAtLeast(0)
        WorkManager.getInstance(context).enqueueUniqueWork(
            NAME, ExistingWorkPolicy.REPLACE,
            OneTimeWorkRequestBuilder<SyncWorker>().setConstraints(online)
                .setInitialDelay(delay, TimeUnit.MILLISECONDS).build(),
        )
    }

    fun cancel(context: Context) = WorkManager.getInstance(context).cancelUniqueWork(NAME)
}
```

`Engram.get`, `engram.drainer()`, `refused` and `pinMismatch` come in Task 8; to compile this task alone, create `Engram.kt` now with just those members (Task 8 fills the rest):

```kotlin
package io.github.overcuriousity.engram.core

import android.content.Context
import io.github.overcuriousity.engram.core.outbox.Drainer
import kotlinx.coroutines.flow.MutableStateFlow

class Engram private constructor(val app: Context) {
    val refused = MutableStateFlow(false)
    val pinMismatch = MutableStateFlow<PinMismatch?>(null)
    internal fun drainer(): Drainer? = null   // Task 8
    companion object {
        @Volatile private var instance: Engram? = null
        fun get(context: Context): Engram =
            instance ?: synchronized(this) { instance ?: Engram(context.applicationContext).also { instance = it } }
    }
}
```

- [ ] **Step 5: Run, expect 7 passed; commit**

```bash
git add android/core
git commit -m "feat(android): the drainer delivers in order and the worker follows the ladder

Evidence: ./gradlew :core:test --tests '*DrainerTest*' — 7 passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 8: Push — the payload, the registration, the service, and the `Engram` façade

**Files:**
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/push/Payload.kt`
- Create: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/push/Push.kt`
- Modify: `android/core/src/main/kotlin/io/github/overcuriousity/engram/core/Engram.kt` (fill in)
- Test: `android/core/src/test/kotlin/io/github/overcuriousity/engram/core/push/PayloadTest.kt`, `PushTest.kt`

**Interfaces:**
- Produces:
  - `sealed class Payload { data class Due(at, moments: List<Moment>, more: Int); data class Notice(at, title, body); data class Unknown(version: Int?) }`; `data class Moment(id, title, at)`; `Payload.parse(bytes: ByteArray): Payload`.
  - `class Push(store: ConnectionStore, transportFor: () -> Transport?, db: Db, clock)` with `suspend fun onEndpoint(endpoint: String, p256dh: String, auth: String, distributor: String)`, `suspend fun onUnregistered()`, `suspend fun received(bytes: ByteArray, decrypted: Boolean): Payload` (parses and caches moments).
  - `Engram` façade: `store: ConnectionStore`, `db: Db`, `outbox: Outbox`, `push: Push`, `situation: Situation`, `counters: ViewCounters`, `userAgent: String`, `deviceName: String`, `refused`, `pinMismatch`, `internal fun transport(): Transport?`, `internal fun drainer(): Drainer?`, `suspend fun pair(uri: PairUri)`, `suspend fun unpair()`, `fun placeOn: Boolean` in plain prefs.

The connector decrypts RFC 8291 itself (`PushMessage.decrypted`) with keys it generates and hands over in `PushEndpoint.pubKeySet`. So the app never touches the cipher: it forwards the connector's public key and auth secret to the server, and parses what arrives.

- [ ] **Step 1: Failing payload tests**

```kotlin
package io.github.overcuriousity.engram.core.push

import org.junit.Assert.*
import org.junit.Test

class PayloadTest {
    @Test fun aVersionOneDueParses() {
        val p = Payload.parse("""{"v":1,"kind":"due","at":1700000000,"moments":[{"id":"m1","title":"Call","at":1700000100}],"more":2}""".toByteArray())
        val d = p as Payload.Due
        assertEquals(1700000000L, d.at); assertEquals(2, d.more)
        assertEquals(Moment("m1", "Call", 1700000100L), d.moments.single())
    }

    @Test fun aNoticeParses() {
        val n = Payload.parse("""{"v":1,"kind":"notice","at":1,"title":"Test","body":"It works"}""".toByteArray()) as Payload.Notice
        assertEquals("Test", n.title); assertEquals("It works", n.body)
    }

    @Test fun aLaterVersionStillRings() {
        assertEquals(Payload.Unknown(2), Payload.parse("""{"v":2,"kind":"due","at":1}""".toByteArray()))
    }

    @Test fun garbageStillRings() {
        assertEquals(Payload.Unknown(null), Payload.parse(byteArrayOf(0, 1, 2)))
        assertEquals(Payload.Unknown(null), Payload.parse("""{"v":1,"kind":"other"}""".toByteArray()))
    }
}
```

- [ ] **Step 2: Failing push tests**

```kotlin
package io.github.overcuriousity.engram.core.push

import io.github.overcuriousity.engram.core.*
import io.github.overcuriousity.engram.core.db.Db
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import java.io.File
import kotlin.io.path.createTempDirectory

class PushTest {
    private val server = MockWebServer()
    private val db = Db.inMemory()
    private lateinit var store: ConnectionStore
    private lateinit var push: Push

    @Before fun up() {
        server.start()
        store = ConnectionStore(File(createTempDirectory("c").toFile(), "conn"), PlainBox())
        store.set(Connection(server.url("/").toString().trimEnd('/'), "tok", null, "1", "d"))
        push = Push(store, { store.current.value?.let { Transport(it, "ua") } }, db) { 42L }
    }
    @After fun down() = server.close()

    @Test fun anEndpointIsSentToTheServerAndRemembered() = runTest {
        server.enqueue(MockResponse(code = 204))
        push.onEndpoint("https://push.test/e", "BPkey", "authsecret", "org.example.dist")
        assertEquals("PUT", server.takeRequest().method)
        assertEquals(PushKeys("https://push.test/e", "BPkey", "authsecret", "org.example.dist"), store.pushKeys)
    }

    @Test fun aServerFailureKeepsTheKeysForRetry() = runTest {
        server.enqueue(MockResponse(code = 503))
        try { push.onEndpoint("https://push.test/e", "k", "a", "d"); fail() } catch (e: java.io.IOException) {}
        assertNotNull(store.pushKeys)
    }

    @Test fun unregisteredDeletesOnTheServerAndForgets() = runTest {
        store.pushKeys = PushKeys("e", "k", "a", "d")
        server.enqueue(MockResponse(code = 204))
        push.onUnregistered()
        assertEquals("DELETE", server.takeRequest().method)
        assertNull(store.pushKeys)
    }

    @Test fun aDuePayloadIsCached() = runTest {
        val p = push.received("""{"v":1,"kind":"due","at":1,"moments":[{"id":"m1","title":"Call","at":9}],"more":0}""".toByteArray(), decrypted = true)
        assertTrue(p is Payload.Due)
        val row = db.momentsDao().latest().first()!!
        assertEquals("m1", row.id); assertEquals(42L, row.fetchedAt)
    }

    @Test fun anUndecryptedMessageIsUnknown() = runTest {
        assertEquals(Payload.Unknown(null), push.received("""{"v":1}""".toByteArray(), decrypted = false))
    }
}
```

- [ ] **Step 3: Run, expect compile failure.**

- [ ] **Step 4: Implement `Payload.kt`**

```kotlin
package io.github.overcuriousity.engram.core.push

import kotlinx.serialization.json.*

data class Moment(val id: String, val title: String, val at: Long)

/** What the server's `jobs::webpush::Payload` becomes on the phone. A version this app does not know still rings. */
sealed class Payload {
    data class Due(val at: Long, val moments: List<Moment>, val more: Int) : Payload()
    data class Notice(val at: Long, val title: String, val body: String) : Payload()
    data class Unknown(val version: Int?) : Payload()

    companion object {
        const val VERSION = 1

        fun parse(bytes: ByteArray): Payload = runCatching {
            val o = Json.parseToJsonElement(String(bytes, Charsets.UTF_8)).jsonObject
            val v = o["v"]?.jsonPrimitive?.intOrNull
            if (v != VERSION) return Unknown(v)
            val at = o["at"]?.jsonPrimitive?.longOrNull ?: 0L
            when (o["kind"]?.jsonPrimitive?.content) {
                "due" -> Due(
                    at,
                    o["moments"]?.jsonArray?.map { m ->
                        val mo = m.jsonObject
                        Moment(mo["id"]!!.jsonPrimitive.content, mo["title"]?.jsonPrimitive?.content ?: "", mo["at"]?.jsonPrimitive?.longOrNull ?: at)
                    } ?: emptyList(),
                    o["more"]?.jsonPrimitive?.intOrNull ?: 0,
                )
                "notice" -> Notice(at, o["title"]?.jsonPrimitive?.content ?: "", o["body"]?.jsonPrimitive?.content ?: "")
                else -> Unknown(null)
            }
        }.getOrDefault(Unknown(null))
    }
}
```

- [ ] **Step 5: Implement `Push.kt`**

```kotlin
package io.github.overcuriousity.engram.core.push

import io.github.overcuriousity.engram.core.ConnectionStore
import io.github.overcuriousity.engram.core.PushKeys
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.MomentRow

/**
 * The server side of a UnifiedPush registration. The connector owns the
 * distributor and the keys; this owns telling the server and reading what
 * comes back.
 */
class Push(
    private val store: ConnectionStore,
    private val transportFor: () -> Transport?,
    private val db: Db,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    /** The keys are kept before the PUT, so a failed PUT can be retried without the distributor's help. */
    suspend fun onEndpoint(endpoint: String, p256dh: String, auth: String, distributor: String) {
        store.pushKeys = PushKeys(endpoint, p256dh, auth, distributor)
        transportFor()?.registerPush(endpoint, p256dh, auth)
    }

    /** Re-send whatever is stored. For the retry worker and for a re-pair. */
    suspend fun resend() {
        val k = store.pushKeys ?: return
        transportFor()?.registerPush(k.endpoint, k.p256dh, k.auth)
    }

    suspend fun onUnregistered() {
        runCatching { transportFor()?.unregisterPush() }
        store.pushKeys = null
    }

    suspend fun received(bytes: ByteArray, decrypted: Boolean): Payload {
        if (!decrypted) return Payload.Unknown(null)
        val p = Payload.parse(bytes)
        if (p is Payload.Due && p.moments.isNotEmpty()) {
            val now = clock()
            db.momentsDao().upsert(p.moments.map { MomentRow(it.id, it.title, it.at, now) })
        }
        return p
    }
}
```

- [ ] **Step 6: Fill in `Engram.kt`**

```kotlin
package io.github.overcuriousity.engram.core

import android.content.Context
import android.os.Build
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.outbox.Drainer
import io.github.overcuriousity.engram.core.outbox.Outbox
import io.github.overcuriousity.engram.core.push.Push
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.flow.MutableStateFlow
import java.io.File
import java.time.ZoneId

/** Everything the app and its receivers are allowed to touch, built once. */
class Engram private constructor(val app: Context, versionName: String) {
    val userAgent = userAgent(versionName, Build.MODEL)
    val deviceName = "engram for Android $versionName · ${Build.MODEL}"
    val store = ConnectionStore(File(app.filesDir, "connection"), KeystoreBox())
    val db = Db.open(app)
    val outbox = Outbox(db, File(app.filesDir, "outbox"))
    val push = Push(store, { transport() }, db)
    private val prefs = app.getSharedPreferences("engram", Context.MODE_PRIVATE)
    val counters = ViewCounters(prefs)
    val situation = Situation(AndroidSituationSource(app, counters), Stable.of(app))

    val refused = MutableStateFlow(false)
    val pinMismatch = MutableStateFlow<PinMismatch?>(null)

    var placeOn: Boolean
        get() = prefs.getBoolean("place", false)
        set(v) = prefs.edit().putBoolean("place", v).apply()

    internal fun transport(): Transport? = store.current.value?.let { Transport(it, userAgent) }
    internal fun drainer(): Drainer? = transport()?.let { Drainer(outbox, it, { ZoneId.systemDefault().id }, System::currentTimeMillis) }

    suspend fun pair(uri: PairUri) {
        val c = Pairing.claim(uri, deviceName, userAgent)
        store.set(c)
        refused.value = false
        pinMismatch.value = null
        outbox.requeueRefused()
        push.resend()
        Sync.kick(app)
    }

    suspend fun unpair() {
        Sync.cancel(app)
        push.onUnregistered()
        store.clear()
        refused.value = false
        pinMismatch.value = null
    }

    companion object {
        @Volatile private var instance: Engram? = null
        fun get(context: Context): Engram = instance ?: synchronized(this) {
            instance ?: run {
                val ctx = context.applicationContext
                val v = ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName ?: "0"
                Engram(ctx, v).also { instance = it }
            }
        }
    }
}
```

`push.resend()` failing on a re-pair must not fail the pairing; wrap it in `runCatching`.

- [ ] **Step 7: Run all core tests, expect 4 + 5 new passed and nothing else broken; commit**

```bash
git add android/core
git commit -m "feat(android): push payload, server registration, and the Engram façade

Evidence: ./gradlew :core:test — N passed.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 9: The theme and the shell

**Files:**
- Create: `android/app/src/main/kotlin/io/github/overcuriousity/engram/ui/Theme.kt`
- Create: `android/app/src/main/res/font/inter_400.ttf` etc. — see step 1
- Create: `android/app/src/main/res/drawable/wordmark.xml`
- Modify: `android/app/src/main/kotlin/io/github/overcuriousity/engram/App.kt`, `MainActivity.kt`
- Create: `android/app/src/main/kotlin/io/github/overcuriousity/engram/ui/Nav.kt`

**Interfaces:**
- Produces: `@Composable fun EngramTheme(content)`; `object EngramColors` (light and dark `ColorScheme`s); `val EngramType: Typography`; `val EngramShapes: Shapes`; `sealed class Screen(route) { Pair, Compose, Queue, Settings }`; `@Composable fun EngramApp(engram: Engram, start: Screen?)` with the banners; `App.engram`.

- [ ] **Step 1: Fonts**

Android's font resources take `.ttf` or `.otf`, not `.woff2`. Convert the four in `assets/fonts/`:

```bash
pip install --user fonttools brotli 2>/dev/null
mkdir -p android/app/src/main/res/font
for f in inter-400 inter-500 inter-600 jetbrains-mono-400; do
  python3 -c "from fontTools.ttLib import TTFont; f=TTFont('assets/fonts/$f.woff2'); f.flavor=None; f.save('android/app/src/main/res/font/${f//-/_}.ttf')"
done
ls android/app/src/main/res/font
```

Resource names must be lowercase with underscores: `inter_400.ttf`, `inter_500.ttf`, `inter_600.ttf`, `jetbrains_mono_400.ttf`.

- [ ] **Step 2: The wordmark**

Open `assets/wordmark.svg`, and write `res/drawable/wordmark.xml` as a `<vector>` with the same `viewportWidth`/`viewportHeight` and each `<path d>` as `android:pathData`, fill `?attr/colorOnSurface` replaced by a literal `#2d2d2d` with `android:tint="?attr/colorOnBackground"` on the vector. If the SVG uses features a vector drawable lacks (text, filters), rasterise instead: `rsvg-convert -h 96 assets/wordmark.svg > res/drawable-xxhdpi/wordmark.png`, and name the resource the same.

- [ ] **Step 3: `Theme.kt`**

```kotlin
package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.github.overcuriousity.engram.R

/** assets/css/00-tokens.css, copied — not approximated. */
object EngramColors {
    val light = lightColorScheme(
        background = Color(0xFFF8F6F1), surface = Color(0xFFF2F0EA), surfaceVariant = Color(0xFFFFFFFF),
        surfaceContainer = Color(0xFFECE9E2), surfaceContainerHigh = Color(0xFFE2DED3),
        onBackground = Color(0xFF2D2D2D), onSurface = Color(0xFF2D2D2D), onSurfaceVariant = Color(0xFF5A5A5A),
        outline = Color(0xFFDDD8CC), outlineVariant = Color(0xFFEAE7DE),
        primary = Color(0xFF386889), onPrimary = Color(0xFFFFFFFF), primaryContainer = Color(0x1A386889),
        error = Color(0xFFB3382C), tertiary = Color(0xFF2B7048), secondary = Color(0xFF845B16),
    )
    val dark = darkColorScheme(
        background = Color(0xFF0E1015), surface = Color(0xFF14171D), surfaceVariant = Color(0xFF1B1E26),
        surfaceContainer = Color(0xFF262A34), surfaceContainerHigh = Color(0xFF2D3140),
        onBackground = Color(0xFFE2E4EC), onSurface = Color(0xFFE2E4EC), onSurfaceVariant = Color(0xFF9599B0),
        outline = Color(0xFF232636), outlineVariant = Color(0xFF191C26),
        primary = Color(0xFF5AA8B0), onPrimary = Color(0xFF0E1015), primaryContainer = Color(0x265AA8B0),
        error = Color(0xFFE77676), tertiary = Color(0xFF4CAF7D), secondary = Color(0xFFE8A839),
    )
    /** `--color-fg-muted` and `--color-due`, which Material has no slot for. */
    val mutedLight = Color(0xFF6C6C65); val mutedDark = Color(0xFF8185A3)
    val dueLight = Color(0xFFA15A1A); val dueDark = Color(0xFFE0A060)
}

val Inter = FontFamily(
    Font(R.font.inter_400, FontWeight.Normal), Font(R.font.inter_500, FontWeight.Medium), Font(R.font.inter_600, FontWeight.SemiBold),
)
val Mono = FontFamily(Font(R.font.jetbrains_mono_400, FontWeight.Normal))

/** The type scale: 0.75, 0.8125, 0.875, 0.9375, 1.125, 1.375, 1.75 rem at 16 px. */
val EngramType = Typography(
    labelSmall = TextStyle(fontFamily = Inter, fontSize = 12.sp),
    bodySmall = TextStyle(fontFamily = Inter, fontSize = 13.sp),
    bodyMedium = TextStyle(fontFamily = Inter, fontSize = 14.sp),
    bodyLarge = TextStyle(fontFamily = Inter, fontSize = 15.sp),
    titleMedium = TextStyle(fontFamily = Inter, fontSize = 18.sp, fontWeight = FontWeight.Medium),
    titleLarge = TextStyle(fontFamily = Inter, fontSize = 22.sp, fontWeight = FontWeight.SemiBold),
    headlineMedium = TextStyle(fontFamily = Inter, fontSize = 28.sp, fontWeight = FontWeight.SemiBold),
    labelMedium = TextStyle(fontFamily = Mono, fontSize = 13.sp),
)

val EngramShapes = Shapes(small = RoundedCornerShape(3.dp), medium = RoundedCornerShape(6.dp), large = RoundedCornerShape(6.dp))

@Composable
fun muted(): Color = if (isSystemInDarkTheme()) EngramColors.mutedDark else EngramColors.mutedLight

@Composable
fun EngramTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = if (isSystemInDarkTheme()) EngramColors.dark else EngramColors.light,
        typography = EngramType,
        shapes = EngramShapes,
        content = content,
    )
}
```

- [ ] **Step 4: `Nav.kt` — the shell and the banners**

```kotlin
package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.compose.*
import io.github.overcuriousity.engram.R
import io.github.overcuriousity.engram.core.Engram

sealed class Screen(val route: String, val label: String) {
    object Pair : Screen("pair", "Pair")
    object Compose : Screen("compose", "Capture")
    object Queue : Screen("queue", "Queue")
    object Settings : Screen("settings", "Settings")
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun EngramApp(engram: Engram, start: Screen? = null, pairText: String? = null) {
    val connection by engram.store.current.collectAsStateWithLifecycle()
    val refused by engram.refused.collectAsStateWithLifecycle()
    val pinned by engram.pinMismatch.collectAsStateWithLifecycle()
    val nav = rememberNavController()

    if (pinned != null) { PinMismatchScreen(pinned!!, onUnpair = { engram.unpairAsync() }); return }
    if (connection == null) { PairScreen(engram, initialText = pairText); return }

    Scaffold(
        topBar = {
            TopAppBar(title = { Image(painterResource(R.drawable.wordmark), contentDescription = "engram", modifier = Modifier.height(20.dp)) })
        },
        bottomBar = {
            NavigationBar {
                listOf(Screen.Compose, Screen.Queue, Screen.Settings).forEach { s ->
                    val current = nav.currentBackStackEntryAsState().value?.destination?.route
                    NavigationBarItem(selected = current == s.route, onClick = { nav.navigate(s.route) { launchSingleTop = true } },
                        icon = {}, label = { Text(s.label) })
                }
            }
        },
    ) { pad ->
        Column(Modifier.padding(pad)) {
            if (refused) RefusedBanner(onRescan = { nav.navigate(Screen.Pair.route) })
            NavHost(nav, startDestination = (start ?: Screen.Compose).route) {
                composable(Screen.Compose.route) { ComposeScreen(engram) }
                composable(Screen.Queue.route) { QueueScreen(engram) }
                composable(Screen.Settings.route) { SettingsScreen(engram) }
                composable(Screen.Pair.route) { PairScreen(engram, initialText = null, knownOrigin = connection?.origin) }
            }
        }
    }
}

@Composable
fun RefusedBanner(onRescan: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.error.copy(alpha = 0.1f), modifier = Modifier.fillMaxWidth()) {
        Row(Modifier.padding(12.dp), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
            Text("Unpaired on the server · queue holds", style = MaterialTheme.typography.bodySmall)
            TextButton(onClick = onRescan) { Text("Scan a new code") }
        }
    }
}

@Composable
fun PinMismatchScreen(e: io.github.overcuriousity.engram.core.PinMismatch, onUnpair: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.Center) {
        Text("Certificate changed", style = MaterialTheme.typography.titleLarge, color = MaterialTheme.colorScheme.error)
        Spacer(Modifier.height(12.dp))
        Text("Pinned", style = MaterialTheme.typography.labelSmall); Text(e.expected, style = MaterialTheme.typography.labelMedium)
        Spacer(Modifier.height(8.dp))
        Text("Served", style = MaterialTheme.typography.labelSmall); Text(e.served, style = MaterialTheme.typography.labelMedium)
        Spacer(Modifier.height(24.dp))
        Button(onClick = onUnpair, colors = ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error)) { Text("Unpair") }
    }
}
```

`engram.unpairAsync()` is an extension in `App.kt` that launches `unpair()` on the application scope. `PairScreen`, `ComposeScreen`, `QueueScreen`, `SettingsScreen` are Tasks 10 to 12; to compile this task, create each as a stub file with a `Text(label)` body and replace it in its task.

- [ ] **Step 5: `App.kt` and `MainActivity.kt`**

```kotlin
package io.github.overcuriousity.engram

import android.app.Application
import io.github.overcuriousity.engram.core.Engram
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

class App : Application() {
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    lateinit var engram: Engram
    override fun onCreate() {
        super.onCreate()
        engram = Engram.get(this)
    }
}

fun Engram.unpairAsync() {
    (app as App).scope.launch { unpair() }
}
```

```kotlin
package io.github.overcuriousity.engram

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import io.github.overcuriousity.engram.core.LightSample
import io.github.overcuriousity.engram.ui.EngramApp
import io.github.overcuriousity.engram.ui.EngramTheme
import io.github.overcuriousity.engram.ui.Screen

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val engram = (application as App).engram
        val pairText = intent?.data?.toString()?.takeIf { it.startsWith("engram://pair") }
        val start = when (intent?.getStringExtra("screen")) { "queue" -> Screen.Queue; "settings" -> Screen.Settings; else -> null }
        setContent { EngramTheme { EngramApp(engram, start, pairText) } }
    }
    override fun onNewIntent(intent: Intent) { super.onNewIntent(intent); setIntent(intent); recreate() }
    override fun onResume() { super.onResume(); LightSample.start(this); (application as App).engram.counters.mark() }
    override fun onPause() { super.onPause(); LightSample.stop(this) }
}
```

Add to the manifest's activity, inside it, a second intent filter for the scheme:

```xml
<intent-filter>
    <action android:name="android.intent.action.VIEW" />
    <category android:name="android.intent.category.DEFAULT" />
    <category android:name="android.intent.category.BROWSABLE" />
    <data android:scheme="engram" android:host="pair" />
</intent-filter>
```

- [ ] **Step 6: Build and look**

Run: `./gradlew :app:assembleDebug`. Expected: BUILD SUCCESSFUL. If a device is attached: `adb install -r app/build/outputs/apk/debug/app-debug.apk`, open it, expect the Pair stub on the web's cream or ink background in Inter.

- [ ] **Step 7: Commit**

```bash
git add android/app
git commit -m "feat(android): the theme is the web's tokens, and the shell routes four screens

Evidence: ./gradlew :app:assembleDebug — BUILD SUCCESSFUL.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 10: The Pair screen and the scanner

**Files:**
- Create: `android/app/src/main/kotlin/io/github/overcuriousity/engram/ui/Scanner.kt`
- Replace: `android/app/src/main/kotlin/io/github/overcuriousity/engram/ui/PairScreen.kt`
- Modify: `android/app/src/main/AndroidManifest.xml` (camera permission and feature)

**Interfaces:**
- Consumes: `PairUri.parse`, `engram.pair(uri)`, `ClaimRefused`.
- Produces: `@Composable fun Scanner(onText: (String) -> Unit)`; `@Composable fun PairScreen(engram, initialText: String?, knownOrigin: String? = null)`.

- [ ] **Step 1: Manifest**

```xml
<uses-permission android:name="android.permission.CAMERA" />
<uses-feature android:name="android.hardware.camera.any" android:required="false" />
```

- [ ] **Step 2: `Scanner.kt`**

```kotlin
package io.github.overcuriousity.engram.ui

import android.Manifest
import android.content.pm.PackageManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import com.google.zxing.*
import com.google.zxing.common.HybridBinarizer
import java.util.concurrent.Executors

/** CameraX preview with ZXing on every frame. Calls back once per distinct text. */
@Composable
fun Scanner(onText: (String) -> Unit) {
    val context = LocalContext.current
    val lifecycle = LocalLifecycleOwner.current
    var granted by remember { mutableStateOf(ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) }
    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted = it }
    LaunchedEffect(Unit) { if (!granted) ask.launch(Manifest.permission.CAMERA) }
    if (!granted) { Text("Camera · needed to scan", style = MaterialTheme.typography.bodySmall, color = muted()); return }

    val reader = remember { MultiFormatReader().apply { setHints(mapOf(DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE))) } }
    var last by remember { mutableStateOf<String?>(null) }
    val executor = remember { Executors.newSingleThreadExecutor() }
    DisposableEffect(Unit) { onDispose { executor.shutdown() } }

    AndroidView(modifier = Modifier.fillMaxWidth().height(320.dp), factory = { ctx ->
        val view = PreviewView(ctx)
        val future = ProcessCameraProvider.getInstance(ctx)
        future.addListener({
            val provider = future.get()
            val preview = androidx.camera.core.Preview.Builder().build().also { it.surfaceProvider = view.surfaceProvider }
            val analysis = ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST).build()
            analysis.setAnalyzer(executor) { img -> decode(img, reader)?.let { t -> if (t != last) { last = t; onText(t) } }; img.close() }
            provider.unbindAll()
            provider.bindToLifecycle(lifecycle, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis)
        }, ContextCompat.getMainExecutor(ctx))
        view
    })
}

private fun decode(img: ImageProxy, reader: MultiFormatReader): String? {
    val plane = img.planes[0]
    val buf = plane.buffer; val bytes = ByteArray(buf.remaining()); buf.get(bytes)
    val src = PlanarYUVLuminanceSource(bytes, plane.rowStride, img.height, 0, 0, img.width, img.height, false)
    return try { reader.decodeWithState(BinaryBitmap(HybridBinarizer(src))).text } catch (e: NotFoundException) { null } finally { reader.reset() }
}
```

- [ ] **Step 3: `PairScreen.kt`**

```kotlin
package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.ClaimRefused
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.PairUri
import kotlinx.coroutines.launch

/** The app's minimum server. A QR from an older one is offered, not refused. */
private const val MIN_SERVER = "0.1.0"

@Composable
fun PairScreen(engram: Engram, initialText: String?, knownOrigin: String? = null) {
    val scope = rememberCoroutineScope()
    var text by remember { mutableStateOf(initialText ?: "") }
    var busy by remember { mutableStateOf<String?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var oldServer by remember { mutableStateOf<PairUri?>(null) }

    fun claim(uri: PairUri) {
        busy = uri.origin; error = null
        scope.launch {
            try { engram.pair(uri) }
            catch (e: ClaimRefused) { error = "Code expired or used · press the button again" }
            catch (e: Exception) { error = e.message ?: "Could not reach ${uri.origin}" }
            finally { busy = null }
        }
    }
    fun take(t: String) {
        val uri = PairUri.parse(t)
        if (uri == null) { error = "Not an engram pairing code"; return }
        if (compareVersions(uri.serverVersion, MIN_SERVER) < 0) oldServer = uri else claim(uri)
    }
    LaunchedEffect(initialText) { if (!initialText.isNullOrBlank()) take(initialText) }

    Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("Pair", style = MaterialTheme.typography.titleLarge)
        Text(if (knownOrigin != null) "Open $knownOrigin/ui/app and press the button" else "Open Settings → Pair the app on your engram",
            style = MaterialTheme.typography.bodySmall, color = muted())
        if (busy == null) Scanner(onText = ::take)
        OutlinedTextField(value = text, onValueChange = { text = it }, label = { Text("or paste the code") }, singleLine = false, modifier = Modifier.fillMaxWidth())
        Button(onClick = { take(text) }, enabled = busy == null && text.isNotBlank()) { Text("Pair") }
        busy?.let { Text("Pairing with $it…", style = MaterialTheme.typography.bodySmall) }
        error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
    }

    oldServer?.let { uri ->
        AlertDialog(onDismissRequest = { oldServer = null },
            title = { Text("Server ${uri.serverVersion} · app expects $MIN_SERVER") },
            text = { Text("Some things may not work.") },
            confirmButton = { TextButton(onClick = { oldServer = null; claim(uri) }) { Text("Pair anyway") } },
            dismissButton = { TextButton(onClick = { oldServer = null }) { Text("Cancel") } })
    }
}

/** Dotted numeric compare; anything unparseable is 0. */
internal fun compareVersions(a: String, b: String): Int {
    val pa = a.split('.').map { it.toIntOrNull() ?: 0 }; val pb = b.split('.').map { it.toIntOrNull() ?: 0 }
    for (i in 0 until maxOf(pa.size, pb.size)) {
        val d = (pa.getOrNull(i) ?: 0) - (pb.getOrNull(i) ?: 0)
        if (d != 0) return d
    }
    return 0
}
```

Add a JVM test in `app/src/test/kotlin/.../ui/VersionsTest.kt`: `compareVersions("0.2.0","0.1.0") > 0`, `("0.1.0","0.1.0") == 0`, `("0.1","0.1.0") == 0`, `("x","0.1.0") < 0`.

- [ ] **Step 4: Build, run on a device if attached, pair against a real server**

Run: `./gradlew :app:testDebugUnitTest :app:assembleDebug`. On a device: open `/ui/app` on the laptop, press the button, scan. Expected: *Pairing with https://…*, then the Capture screen; the token appears under Settings → API tokens on the web named `engram for Android 0.1.0 · <model>`.

- [ ] **Step 5: Commit**

```bash
git add android/app
git commit -m "feat(android): the Pair screen scans the code, or takes it pasted, and claims it

Evidence: ./gradlew :app:testDebugUnitTest :app:assembleDebug — 4 passed, BUILD SUCCESSFUL. Paired against <origin> on a <device> / not run on a device.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 11: The Compose and Queue screens

**Files:**
- Replace: `android/app/src/main/kotlin/io/github/overcuriousity/engram/ui/ComposeScreen.kt`, `QueueScreen.kt`
- Create: `android/app/src/main/kotlin/io/github/overcuriousity/engram/doors/Intake.kt`
- Test: `android/app/src/test/kotlin/io/github/overcuriousity/engram/ui/RowWordsTest.kt`

**Interfaces:**
- Produces:
  - `object Intake { suspend fun text(engram, text, title?, note?): String; suspend fun uris(engram, uris: List<Uri>, title?, note?): String }` — copies content URIs through `Incoming` and kicks `Sync`.
  - `fun rowWords(row: OutboxRow, now: Long): String` — the state in words the Queue prints.

- [ ] **Step 1: The words, tested**

```kotlin
package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.db.*
import org.junit.Assert.assertEquals
import org.junit.Test

class RowWordsTest {
    private fun row(state: State, status: Int? = null, error: String? = null, nextAt: Long = 0) =
        OutboxRow("id", Kind.capture_text, "{}", 0, 0, nextAt, state, status, null, error)

    @Test fun eachStateHasItsWords() {
        assertEquals("waiting", rowWords(row(State.queued), now = 10))
        assertEquals("waiting · next try in 2 min", rowWords(row(State.queued, nextAt = 130_000), now = 10_000))
        assertEquals("stored", rowWords(row(State.sent, 201), now = 0))
        assertEquals("already held", rowWords(row(State.sent, 200), now = 0))
        assertEquals("stored · still being read", rowWords(row(State.sent, 202), now = 0))
        assertEquals("held for review · that body is not valid UTF-8 text", rowWords(row(State.held, 400, "that body is not valid UTF-8 text"), now = 0))
        assertEquals("refused · scan a new code", rowWords(row(State.refused), now = 0))
    }
}
```

```kotlin
package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.db.OutboxRow
import io.github.overcuriousity.engram.core.db.State

fun rowWords(row: OutboxRow, now: Long): String = when (row.state) {
    State.queued -> {
        val wait = row.nextAt - now
        if (wait <= 0) "waiting" else "waiting · next try in ${span(wait)}"
    }
    State.sent -> when (row.status) { 200 -> "already held"; 202 -> "stored · still being read"; else -> "stored" }
    State.held -> "held for review · ${row.error ?: ""}".trimEnd(' ', '·')
    State.refused -> "refused · scan a new code"
}

private fun span(ms: Long): String {
    val s = ms / 1000
    return when { s < 90 -> "${s}s"; s < 5400 -> "${(s + 30) / 60} min"; else -> "${(s + 1800) / 3600} h" }
}
```

- [ ] **Step 2: `Intake.kt`**

```kotlin
package io.github.overcuriousity.engram.doors

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.outbox.Incoming
import io.github.overcuriousity.engram.core.sync.Sync

/** Every door lands here. Copies first, answers at once, kicks the worker. */
object Intake {
    suspend fun text(engram: Engram, text: String, title: String? = null, note: String? = null): String {
        val id = engram.outbox.enqueueText(text, title, note)
        Sync.kick(engram.app)
        return id
    }

    suspend fun uris(engram: Engram, uris: List<Uri>, title: String? = null, note: String? = null): String {
        val ctx = engram.app
        val incoming = uris.map { uri ->
            Incoming(displayName(ctx, uri), ctx.contentResolver.getType(uri) ?: "application/octet-stream") {
                ctx.contentResolver.openInputStream(uri) ?: throw java.io.IOException("cannot open $uri")
            }
        }
        val id = engram.outbox.enqueueFiles(incoming, title, note)
        Sync.kick(ctx)
        return id
    }

    private fun displayName(ctx: Context, uri: Uri): String =
        runCatching {
            ctx.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
                if (c.moveToFirst()) c.getString(0) else null
            }
        }.getOrNull() ?: uri.lastPathSegment ?: "file"
}
```

- [ ] **Step 3: `ComposeScreen.kt`**

```kotlin
package io.github.overcuriousity.engram.ui

import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.doors.Intake
import kotlinx.coroutines.launch

@Composable
fun ComposeScreen(engram: Engram, justQueued: String? = null) {
    val scope = rememberCoroutineScope()
    var text by remember { mutableStateOf("") }
    var title by remember { mutableStateOf("") }
    var note by remember { mutableStateOf("") }
    var more by remember { mutableStateOf(false) }
    var files by remember { mutableStateOf(listOf<Uri>()) }
    var confirmed by remember { mutableStateOf(justQueued) }

    val pick = rememberLauncherForActivityResult(ActivityResultContracts.GetMultipleContents()) { files = files + it }
    val photo = rememberLauncherForActivityResult(ActivityResultContracts.TakePicturePreview()) { bmp ->
        bmp ?: return@rememberLauncherForActivityResult
        val f = java.io.File.createTempFile("photo", ".jpg", engram.app.cacheDir)
        f.outputStream().use { bmp.compress(android.graphics.Bitmap.CompressFormat.JPEG, 90, it) }
        files = files + Uri.fromFile(f)
    }

    fun send() {
        val t = text.trim(); val ti = title.trim().ifEmpty { null }; val n = note.trim().ifEmpty { null }
        scope.launch {
            val id = if (files.isNotEmpty()) Intake.uris(engram, files, ti, n ?: t.ifEmpty { null })
                     else Intake.text(engram, t, ti, n)
            text = ""; title = ""; note = ""; files = emptyList(); confirmed = id
        }
    }

    Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        confirmed?.let {
            Text("Queued · see Queue", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.tertiary)
        }
        OutlinedTextField(value = text, onValueChange = { text = it }, modifier = Modifier.fillMaxWidth().weight(1f),
            placeholder = { Text("Paste something to keep…") })
        if (files.isNotEmpty()) LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            items(files) { u -> AssistChip(onClick = { files = files - u }, label = { Text(u.lastPathSegment ?: "file", maxLines = 1) }) }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            TextButton(onClick = { pick.launch("*/*") }) { Text("Attach") }
            TextButton(onClick = { photo.launch(null) }) { Text("Photo") }
            TextButton(onClick = { more = !more }) { Text(if (more) "Less" else "Title · note") }
        }
        if (more) {
            OutlinedTextField(value = title, onValueChange = { title = it }, label = { Text("Title") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            OutlinedTextField(value = note, onValueChange = { note = it }, label = { Text("Note") }, modifier = Modifier.fillMaxWidth())
        }
        Button(onClick = ::send, enabled = text.isNotBlank() || files.isNotEmpty(), modifier = Modifier.fillMaxWidth()) { Text("Keep") }
    }
}
```

The microphone door: a `RecordAudio` button using `MediaRecorder` to an `.m4a` in `cacheDir`, added to `files` on stop. Implement it as `AudioNote.kt` with `start(context): File` and `stop()`, and a third `TextButton("Record" / "Stop")`; it needs `RECORD_AUDIO` in the manifest and a runtime permission request like the camera's in `Scanner`.

- [ ] **Step 4: `QueueScreen.kt`**

```kotlin
package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.State
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.launch
import kotlinx.serialization.json.*

@Composable
fun QueueScreen(engram: Engram) {
    val rows by engram.outbox.rows.collectAsStateWithLifecycle(emptyList())
    val scope = rememberCoroutineScope()
    val now = System.currentTimeMillis()
    Column(Modifier.fillMaxSize()) {
        Row(Modifier.fillMaxWidth().padding(16.dp, 8.dp), horizontalArrangement = Arrangement.SpaceBetween) {
            Text("Queue", style = MaterialTheme.typography.titleLarge)
            TextButton(onClick = { Sync.kick(engram.app) }) { Text("Deliver now") }
        }
        if (rows.isEmpty()) Text("Nothing owed", Modifier.padding(16.dp), color = muted())
        LazyColumn { items(rows, key = { it.id }) { row ->
            ListItem(
                headlineContent = { Text(firstLine(row.kind, row.payload), maxLines = 2) },
                supportingContent = { Text(rowWords(row, now), style = MaterialTheme.typography.bodySmall, color = muted()) },
                trailingContent = { if (row.state == State.held) TextButton(onClick = { scope.launch { engram.outbox.delete(row.id) } }) { Text("Delete") } },
            )
            HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
        } }
    }
}

private fun firstLine(kind: Kind, payload: String): String {
    val p = runCatching { Json.parseToJsonElement(payload).jsonObject }.getOrNull() ?: return kind.name
    return when (kind) {
        Kind.capture_text -> p["text"]?.jsonPrimitive?.content?.lineSequence()?.firstOrNull { it.isNotBlank() } ?: "(text)"
        Kind.capture_files -> p["title"]?.jsonPrimitive?.contentOrNull ?: "Files"
        Kind.done -> "Done · ${p["moment"]?.jsonPrimitive?.content}"
        Kind.snooze -> "Snoozed · ${p["moment"]?.jsonPrimitive?.content}"
    }
}
```

- [ ] **Step 5: Build, test, commit**

Run: `./gradlew :app:testDebugUnitTest :app:assembleDebug` → `RowWordsTest` 1 passed. On a device: type, Keep, see the row go from *waiting* to *stored*; put the phone in airplane mode, Keep, see *waiting · next try in 30s*, come back online, see *stored*.

```bash
git add android/app
git commit -m "feat(android): Capture writes the outbox and Queue reads it, in words

Evidence: ./gradlew :app:testDebugUnitTest — N passed; :app:assembleDebug BUILD SUCCESSFUL.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 12: The Settings screen and the UnifiedPush service

**Files:**
- Replace: `android/app/src/main/kotlin/io/github/overcuriousity/engram/ui/SettingsScreen.kt`
- Create: `android/app/src/main/kotlin/io/github/overcuriousity/engram/push/PushServiceImpl.kt`, `push/PushRegistrar.kt`
- Modify: `android/app/src/main/AndroidManifest.xml`

**Interfaces:**
- Consumes: `UnifiedPush.getDistributors`, `tryUseCurrentOrDefaultDistributor`, `register(context, instance, messageForDistributor, vapid)`, `unregister`; `engram.push`, `engram.placeOn`, `engram.situation`.
- Produces: `object PushRegistrar { fun ensure(activity: Context, engram: Engram) }` — fetches VAPID, picks a distributor, registers; `class PushServiceImpl : PushService`.

- [ ] **Step 1: Manifest**

Inside `<application>`:

```xml
<service android:name=".push.PushServiceImpl" android:exported="false">
    <intent-filter><action android:name="org.unifiedpush.android.connector.PUSH_EVENT" /></intent-filter>
</service>
```

and `<uses-permission android:name="android.permission.POST_NOTIFICATIONS" />`, `<uses-permission android:name="android.permission.ACCESS_COARSE_LOCATION" />`.

- [ ] **Step 2: `PushRegistrar.kt`**

```kotlin
package io.github.overcuriousity.engram.push

import android.content.Context
import io.github.overcuriousity.engram.App
import io.github.overcuriousity.engram.core.Engram
import kotlinx.coroutines.launch
import org.unifiedpush.android.connector.UnifiedPush

/**
 * Pick a distributor and register. The VAPID key is fetched first so the
 * distributor can hand the endpoint a key the server will sign with.
 */
object PushRegistrar {
    fun distributors(context: Context): List<String> = UnifiedPush.getDistributors(context)

    fun ensure(context: Context, engram: Engram, onNoDistributor: () -> Unit) {
        if (UnifiedPush.getDistributors(context).isEmpty()) { onNoDistributor(); return }
        (engram.app as App).scope.launch {
            val vapid = runCatching { engram.vapid() }.getOrNull()
            UnifiedPush.tryUseCurrentOrDefaultDistributor(context) { ok ->
                if (ok) UnifiedPush.register(context, vapid = vapid)
            }
        }
    }

    fun forget(context: Context) = UnifiedPush.unregister(context)
}
```

Add to `Engram`: `suspend fun vapid(): String = transport()?.vapid() ?: throw IllegalStateException("unpaired")`.

- [ ] **Step 3: `PushServiceImpl.kt`**

```kotlin
package io.github.overcuriousity.engram.push

import io.github.overcuriousity.engram.App
import kotlinx.coroutines.runBlocking
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.PushService
import org.unifiedpush.android.connector.UnifiedPush
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage

class PushServiceImpl : PushService() {
    private val engram get() = (application as App).engram

    override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) {
        val keys = endpoint.pubKeySet ?: return   // no keys, no encryption: the server would store a legacy row; refuse to
        val distributor = UnifiedPush.getAckDistributor(this) ?: "?"
        runBlocking { runCatching { engram.push.onEndpoint(endpoint.url, keys.pubKey, keys.auth, distributor) } }
        PushRetry.schedule(this)   // Task 7's Sync pattern: re-PUT until it lands
    }

    override fun onMessage(message: PushMessage, instance: String) {
        val payload = runBlocking { engram.push.received(message.content, message.decrypted) }
        Reminders.show(this, payload)   // Task 14
    }

    override fun onRegistrationFailed(reason: FailedReason, instance: String) {
        engram.pushFailure.value = reason.name
    }

    override fun onUnregistered(instance: String) {
        runBlocking { engram.push.onUnregistered() }
    }
}
```

Add `val pushFailure = MutableStateFlow<String?>(null)` to `Engram`. `PushRetry` is a `CoroutineWorker` in `push/PushRetry.kt` with unique name `"engram-push"`, network constraint, `BackoffPolicy.EXPONENTIAL` 30 s, that calls `engram.push.resend()` and returns `Result.retry()` on `IOException`.

- [ ] **Step 4: `SettingsScreen.kt`**

```kotlin
package io.github.overcuriousity.engram.ui

import android.Manifest
import android.content.Intent
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.push.PushRegistrar
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

@Composable
fun SettingsScreen(engram: Engram) {
    val ctx = LocalContext.current
    val c by engram.store.current.collectAsStateWithLifecycle()
    val latest by engram.db.momentsDao().latest().collectAsStateWithLifecycle(null)
    val pushFailure by engram.pushFailure.collectAsStateWithLifecycle()
    var noDistributor by remember { mutableStateOf(false) }
    var placeOn by remember { mutableStateOf(engram.placeOn) }
    var confirmUnpair by remember { mutableStateOf(false) }
    val askLocation = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { ok -> placeOn = ok; engram.placeOn = ok }
    val askNotify = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {}
    LaunchedEffect(Unit) { askNotify.launch(Manifest.permission.POST_NOTIFICATIONS) }

    Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
        Text("Settings", style = MaterialTheme.typography.titleLarge)
        Section("Server") {
            Line(c?.origin ?: "—"); Line("version ${c?.serverVersion ?: "—"}", muted = true)
            Line(c?.deviceName ?: "", muted = true)
            Line(if (c?.pin != null) "pinned · ${c!!.pin!!.take(12)}…" else "public certificate · no pin", muted = true)
        }
        Section("Reminders") {
            val keys = engram.store.pushKeys
            when {
                keys != null -> Line("registered · ${keys.distributor}", muted = true)
                noDistributor -> {
                    Line("No UnifiedPush distributor", muted = true)
                    Line("Reminders need one. ntfy or NextPush, from F-Droid.", muted = true)
                    TextButton(onClick = { ctx.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse("https://unifiedpush.org/users/distributors/"))) }) { Text("Distributors") }
                }
                else -> Button(onClick = { PushRegistrar.ensure(ctx, engram) { noDistributor = true } }) { Text("Register for reminders") }
            }
            pushFailure?.let { Line("failed · $it", muted = true) }
            latest?.let { Line("last · ${it.title} · ${stamp(it.at)}", muted = true) }
        }
        Section("Place") {
            Row(verticalAlignment = androidx.compose.ui.Alignment.CenterVertically, horizontalArrangement = Arrangement.SpaceBetween, modifier = Modifier.fillMaxWidth()) {
                Column { Line("Send my place"); Line("a one-kilometre cell · never finer", muted = true) }
                Switch(checked = placeOn, onCheckedChange = { on ->
                    if (on) askLocation.launch(Manifest.permission.ACCESS_COARSE_LOCATION) else { placeOn = false; engram.placeOn = false }
                })
            }
        }
        Section("This phone") {
            Line(engram.situation.bundle(placeOn).toString(), muted = true, mono = true)
        }
        OutlinedButton(onClick = { confirmUnpair = true }, colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error)) { Text("Unpair") }
    }
    if (confirmUnpair) AlertDialog(onDismissRequest = { confirmUnpair = false }, title = { Text("Unpair this phone?") },
        text = { Text("Queued captures stay until you pair again.") },
        confirmButton = { TextButton(onClick = { confirmUnpair = false; PushRegistrar.forget(ctx); engram.unpairAsync() }) { Text("Unpair") } },
        dismissButton = { TextButton(onClick = { confirmUnpair = false }) { Text("Keep") } })
}

@Composable private fun Section(title: String, content: @Composable ColumnScope.() -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) { Text(title, style = MaterialTheme.typography.titleMedium); content() }
}
@Composable private fun Line(text: String, muted: Boolean = false, mono: Boolean = false) {
    Text(text, style = if (mono) MaterialTheme.typography.labelMedium else MaterialTheme.typography.bodyMedium,
        color = if (muted) muted() else MaterialTheme.colorScheme.onBackground)
}
private fun stamp(at: Long) = DateTimeFormatter.ofPattern("dd.MM., HH:mm").format(Instant.ofEpochSecond(at).atZone(ZoneId.systemDefault()))
```

`engram.unpairAsync()` is imported from the app package. The *This phone* line shows the live bundle; it is how the situation source is checked on a device, since it has no unit test.

- [ ] **Step 5: Build, run, register**

On a device with ntfy installed: Settings → Register for reminders → pick ntfy. Expected: *registered · io.heckel.ntfy*; on the web, Settings shows *Registered by engram-android/0.1.0 (<model>) · encrypted*; *Test UnifiedPush* on the web rings the phone (Task 14 draws it; until then check `adb logcat` for the payload).

- [ ] **Step 6: Commit**

```bash
git add android/app android/core
git commit -m "feat(android): Settings shows the server, registers for reminders, and holds the place switch

Evidence: ./gradlew :app:assembleDebug — BUILD SUCCESSFUL. Registered with <distributor> on a <device> / not run on a device.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 13: The doors — share, process-text, tile, shortcut

**Files:**
- Create: `android/app/src/main/kotlin/io/github/overcuriousity/engram/doors/ShareActivity.kt`, `doors/CaptureTile.kt`
- Create: `android/app/src/main/res/xml/shortcuts.xml`
- Modify: `android/app/src/main/AndroidManifest.xml`
- Test: `android/app/src/androidTest/kotlin/io/github/overcuriousity/engram/doors/ShareIntakeTest.kt`

**Interfaces:**
- Consumes: `Intake.text`, `Intake.uris`.
- Produces: `ShareActivity` (translucent, finishes at once), `CaptureTile : TileService`, the launcher shortcut `compose`.

- [ ] **Step 1: Manifest additions inside `<application>`**

```xml
<activity android:name=".doors.ShareActivity" android:exported="true" android:excludeFromRecents="true"
          android:theme="@android:style/Theme.Translucent.NoTitleBar" android:label="Save to engram">
    <intent-filter>
        <action android:name="android.intent.action.SEND" />
        <category android:name="android.intent.category.DEFAULT" />
        <data android:mimeType="text/*" /><data android:mimeType="image/*" />
        <data android:mimeType="application/pdf" /><data android:mimeType="*/*" />
    </intent-filter>
    <intent-filter>
        <action android:name="android.intent.action.SEND_MULTIPLE" />
        <category android:name="android.intent.category.DEFAULT" />
        <data android:mimeType="*/*" />
    </intent-filter>
    <intent-filter>
        <action android:name="android.intent.action.PROCESS_TEXT" />
        <category android:name="android.intent.category.DEFAULT" />
        <data android:mimeType="text/plain" />
    </intent-filter>
</activity>
<service android:name=".doors.CaptureTile" android:exported="true" android:icon="@drawable/ic_tile"
         android:label="engram" android:permission="android.permission.BIND_QUICK_SETTINGS_TILE">
    <intent-filter><action android:name="android.service.quicksettings.action.QS_TILE" /></intent-filter>
</service>
```

and on `MainActivity`: `<meta-data android:name="android.app.shortcuts" android:resource="@xml/shortcuts" />`.

`res/drawable/ic_tile.xml`: a 24 dp vector of the logo (`assets/logo.svg` converted as in Task 9) or, failing that, a plain circle.

- [ ] **Step 2: `ShareActivity.kt`**

```kotlin
package io.github.overcuriousity.engram.doors

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.lifecycle.lifecycleScope
import io.github.overcuriousity.engram.App
import kotlinx.coroutines.launch

/**
 * Every share lands here and leaves at once. Unpaired, it says so and opens
 * the app; paired, it copies, enqueues, toasts, and finishes — the sending
 * app never sees a screen of ours.
 */
class ShareActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val engram = (application as App).engram
        if (engram.store.current.value == null) {
            Toast.makeText(this, "Pair engram first", Toast.LENGTH_SHORT).show()
            startActivity(Intent(this, io.github.overcuriousity.engram.MainActivity::class.java))
            finish(); return
        }
        val i = intent
        val title = i.getStringExtra(Intent.EXTRA_SUBJECT) ?: i.getStringExtra(Intent.EXTRA_TITLE)
        lifecycleScope.launch {
            try {
                when (i.action) {
                    Intent.ACTION_PROCESS_TEXT -> {
                        val t = i.getCharSequenceExtra(Intent.EXTRA_PROCESS_TEXT)?.toString().orEmpty()
                        if (t.isNotBlank()) Intake.text(engram, t)
                    }
                    Intent.ACTION_SEND -> {
                        val stream = i.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
                        val text = i.getStringExtra(Intent.EXTRA_TEXT)
                        when {
                            stream != null -> Intake.uris(engram, listOf(stream), title, text)
                            !text.isNullOrBlank() -> Intake.text(engram, text, title)
                        }
                    }
                    Intent.ACTION_SEND_MULTIPLE -> {
                        val streams = i.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
                        if (streams.isNotEmpty()) Intake.uris(engram, streams, title, i.getStringExtra(Intent.EXTRA_TEXT))
                    }
                }
                Toast.makeText(this@ShareActivity, "Kept · engram", Toast.LENGTH_SHORT).show()
            } catch (e: Exception) {
                Toast.makeText(this@ShareActivity, "Could not read that: ${e.message}", Toast.LENGTH_LONG).show()
            } finally { finish() }
        }
    }
}
```

- [ ] **Step 3: `CaptureTile.kt` and `shortcuts.xml`**

```kotlin
package io.github.overcuriousity.engram.doors

import android.app.PendingIntent
import android.content.Intent
import android.service.quicksettings.TileService
import io.github.overcuriousity.engram.MainActivity

class CaptureTile : TileService() {
    override fun onClick() {
        val i = Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        startActivityAndCollapse(PendingIntent.getActivity(this, 0, i, PendingIntent.FLAG_IMMUTABLE))
    }
}
```

```xml
<shortcuts xmlns:android="http://schemas.android.com/apk/res/android">
    <shortcut android:shortcutId="compose" android:enabled="true" android:icon="@drawable/ic_tile"
              android:shortcutShortLabel="@string/shortcut_compose">
        <intent android:action="android.intent.action.MAIN" android:targetPackage="io.github.overcuriousity.engram"
                android:targetClass="io.github.overcuriousity.engram.MainActivity" />
    </shortcut>
</shortcuts>
```

with `res/values/strings.xml` holding `<string name="shortcut_compose">Capture</string>`.

- [ ] **Step 4: Instrumentation test for the share path**

```kotlin
package io.github.overcuriousity.engram.doors

import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.core.content.FileProvider
import io.github.overcuriousity.engram.App
import io.github.overcuriousity.engram.core.db.Kind
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class ShareIntakeTest {
    @Test fun aContentUriIsCopiedIntoTheOutbox() = runBlocking {
        val app = ApplicationProvider.getApplicationContext<App>()
        val src = File(app.cacheDir, "shared.txt").apply { writeText("shared bytes") }
        val uri = FileProvider.getUriForFile(app, "${app.packageName}.files", src)
        val id = Intake.uris(app.engram, listOf(uri), "t", null)
        src.delete()   // the sender's file is gone; ours must not be
        val row = app.engram.outbox.rows.first().first { it.id == id }
        assertEquals(Kind.capture_files, row.kind)
        val f = app.engram.outbox.filesOf(id).single()
        assertEquals("shared bytes", File(f.path).readText())
    }
}
```

This needs a `FileProvider` declared in the manifest with authority `${applicationId}.files` and `res/xml/paths.xml` exposing `cache-path`. Add `androidTestImplementation(libs.androidx.test.junit)` and `androidx.test:core` to `app`.

- [ ] **Step 5: Build; share from another app on a device**

Run: `./gradlew :app:assembleDebug`. On a device: share a photo from Gallery → *Save to engram* → toast *Kept · engram*; select text in any app → *Save to engram*; pull down quick settings → add the engram tile → tap. If a device is attached: `./gradlew :app:connectedDebugAndroidTest`.

- [ ] **Step 6: Commit**

```bash
git add android/app
git commit -m "feat(android): every door writes the outbox — share, selection, tile, shortcut

Evidence: ./gradlew :app:assembleDebug — BUILD SUCCESSFUL; connectedDebugAndroidTest 1 passed / not run, no device.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 14: Reminders ring, with done and snooze

**Files:**
- Create: `android/app/src/main/kotlin/io/github/overcuriousity/engram/push/Reminders.kt`, `push/ActionReceiver.kt`
- Modify: `android/app/src/main/AndroidManifest.xml`
- Test: `android/app/src/test/kotlin/io/github/overcuriousity/engram/push/RemindersTextTest.kt`

**Interfaces:**
- Produces: `object Reminders { fun ensureChannel(context); fun show(context, payload: Payload); fun lines(payload): Pair<String, String> }`; `class ActionReceiver : BroadcastReceiver` handling `io.github.overcuriousity.engram.DONE` and `.SNOOZE` with extra `moment`.

- [ ] **Step 1: The text, tested**

```kotlin
package io.github.overcuriousity.engram.push

import io.github.overcuriousity.engram.core.push.Moment
import io.github.overcuriousity.engram.core.push.Payload
import org.junit.Assert.assertEquals
import org.junit.Test

class RemindersTextTest {
    @Test fun dueListsTheMomentsAndCountsTheRest() {
        val (title, body) = Reminders.lines(Payload.Due(0, listOf(Moment("a", "Call Sam", 0), Moment("b", "Pay rent", 0)), 3))
        assertEquals("Due", title)
        assertEquals("Call Sam\nPay rent\n+3 more", body)
    }
    @Test fun aSingleMomentIsTheTitle() {
        assertEquals("Call Sam" to "", Reminders.lines(Payload.Due(0, listOf(Moment("a", "Call Sam", 0)), 0)))
    }
    @Test fun noticeIsItself() { assertEquals("Test" to "It works", Reminders.lines(Payload.Notice(0, "Test", "It works"))) }
    @Test fun unknownStillRings() {
        assertEquals("Something is due" to "This app is behind the server · update it", Reminders.lines(Payload.Unknown(2)))
        assertEquals("Something is due" to "This app is behind the server · update it", Reminders.lines(Payload.Unknown(null)))
    }
}
```

- [ ] **Step 2: `Reminders.kt`**

```kotlin
package io.github.overcuriousity.engram.push

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import io.github.overcuriousity.engram.MainActivity
import io.github.overcuriousity.engram.R
import io.github.overcuriousity.engram.core.push.Payload

object Reminders {
    const val CHANNEL = "reminders"
    const val ACTION_DONE = "io.github.overcuriousity.engram.DONE"
    const val ACTION_SNOOZE = "io.github.overcuriousity.engram.SNOOZE"

    fun ensureChannel(context: Context) {
        val nm = context.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "Reminders", NotificationManager.IMPORTANCE_HIGH))
    }

    fun lines(p: Payload): Pair<String, String> = when (p) {
        is Payload.Due -> when {
            p.moments.size == 1 && p.more == 0 -> p.moments[0].title to ""
            else -> "Due" to (p.moments.map { it.title } + (if (p.more > 0) listOf("+${p.more} more") else emptyList())).joinToString("\n")
        }
        is Payload.Notice -> p.title to p.body
        is Payload.Unknown -> "Something is due" to "This app is behind the server · update it"
    }

    fun show(context: Context, p: Payload) {
        val (title, body) = lines(p)
        val open = PendingIntent.getActivity(context, 0, Intent(context, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
        val b = NotificationCompat.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.ic_tile).setContentTitle(title)
            .setStyle(NotificationCompat.BigTextStyle().bigText(body)).setContentText(body.lineSequence().firstOrNull() ?: "")
            .setContentIntent(open).setAutoCancel(true).setPriority(NotificationCompat.PRIORITY_HIGH)
        if (p is Payload.Due && p.moments.isNotEmpty()) {
            val first = p.moments[0]
            b.addAction(0, "Done", action(context, ACTION_DONE, first.id))
            b.addAction(0, "Snooze 1 h", action(context, ACTION_SNOOZE, first.id))
        }
        context.getSystemService(NotificationManager::class.java).notify(notificationId(p), b.build())
    }

    private fun action(context: Context, action: String, moment: String): PendingIntent =
        PendingIntent.getBroadcast(context, moment.hashCode(),
            Intent(context, ActionReceiver::class.java).setAction(action).putExtra("moment", moment),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)

    /** One notification per push, keyed by its instant, so a ladder rung replaces the previous one. */
    private fun notificationId(p: Payload) = when (p) { is Payload.Due -> (p.at % Int.MAX_VALUE).toInt(); is Payload.Notice -> (p.at % Int.MAX_VALUE).toInt(); else -> 1 }
}
```

`NotificationCompat` needs `androidx.core:core-ktx`, already a dependency.

- [ ] **Step 3: `ActionReceiver.kt` and manifest**

```kotlin
package io.github.overcuriousity.engram.push

import android.app.NotificationManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import io.github.overcuriousity.engram.App
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.launch

/** Done and snooze are writes the device owes the server: outbox rows, like any capture. */
class ActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val app = context.applicationContext as App
        val moment = intent.getStringExtra("moment") ?: return
        val pending = goAsync()
        app.scope.launch {
            try {
                when (intent.action) {
                    Reminders.ACTION_DONE -> app.engram.outbox.enqueueDone(moment)
                    Reminders.ACTION_SNOOZE -> app.engram.outbox.enqueueSnooze(moment, System.currentTimeMillis() / 1000 + 3600)
                }
                Sync.kick(context)
                context.getSystemService(NotificationManager::class.java).cancelAll()
            } finally { pending.finish() }
        }
    }
}
```

Manifest: `<receiver android:name=".push.ActionReceiver" android:exported="false" />`. In `App.onCreate`, after `engram = Engram.get(this)`, add `Reminders.ensureChannel(this)`.

- [ ] **Step 4: Build, test, ring**

Run: `./gradlew :app:testDebugUnitTest :app:assembleDebug` → 4 passed. On a device: web Settings → *Test UnifiedPush* → the phone shows *Test* with the body; set a reminder due in a minute on the web → *Call Sam* with *Done* and *Snooze 1 h*; press Done → Queue shows *Done · <id>* going *stored*; the web's reminder is done.

- [ ] **Step 5: Commit**

```bash
git add android/app
git commit -m "feat(android): a push rings, with done and snooze written to the outbox

Evidence: ./gradlew :app:testDebugUnitTest — N passed; rang on a <device> from the web's test button / not run on a device.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 15: The outbox on a device, and CI

**Files:**
- Create: `android/core/src/androidTest/kotlin/io/github/overcuriousity/engram/core/outbox/OutboxDeviceTest.kt`
- Create: `.github/workflows/android.yml`
- Modify: `android/README.md`

**Interfaces:**
- Consumes: `Db.open`, `Outbox`, `Drainer`, `Transport`, `KeystoreBox`, `ConnectionStore`.

- [ ] **Step 1: The device tests**

```kotlin
package io.github.overcuriousity.engram.core.outbox

import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.github.overcuriousity.engram.core.*
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.State
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class OutboxDeviceTest {
    private val ctx = ApplicationProvider.getApplicationContext<android.content.Context>()

    @Test fun acceptedOfflineDeliveredOnReconnectBytesIntact() = runBlocking {
        val dir = File(ctx.cacheDir, "outbox-test").apply { deleteRecursively(); mkdirs() }
        val db = Db.open(ctx)
        val box = Outbox(db, dir)
        val id = box.enqueueFiles(listOf(Incoming("a.bin", "application/octet-stream") { ByteArray(4096) { it.toByte() }.inputStream() }), null, null)
        // "Offline": a transport pointed at a closed port.
        val dead = Transport(Connection("http://127.0.0.1:1", "t", null, "1", "d"), "ua")
        assertTrue(Drainer(box, dead, { "UTC" }, System::currentTimeMillis).drainOnce() is Drainer.Outcome.Later)
        assertEquals(State.queued, box.rows.first().first { it.id == id }.state)
        // Reconnect.
        val server = MockWebServer().apply { start(); enqueue(MockResponse(code = 201, body = "{}")) }
        val live = Transport(Connection(server.url("/").toString().trimEnd('/'), "t", null, "1", "d"), "ua")
        val due = Outbox(db, dir) { System.currentTimeMillis() + 60_000 }   // past the first rung
        Drainer(due, live, { "UTC" }, { System.currentTimeMillis() + 60_000 }).drainOnce()
        assertEquals(State.sent, box.rows.first().first { it.id == id }.state)
        val body = server.takeRequest().body!!.readByteArray()
        assertTrue(body.size > 4096)   // multipart framing plus the 4096 bytes, byte-exact inside
        assertTrue(body.toList().windowed(4096).any { w -> w.withIndex().all { (i, b) -> b == i.toByte() } })
        server.close()
    }

    @Test fun theKeystoreBoxRoundTrips() {
        val f = File(ctx.filesDir, "conn-test").apply { delete() }
        val c = Connection("https://x", "engram_secret", "pin", "1", "d")
        ConnectionStore(f, KeystoreBox("engram-test")).set(c)
        assertEquals(c, ConnectionStore(f, KeystoreBox("engram-test")).current.value)
        assertFalse(f.readText(Charsets.ISO_8859_1).contains("engram_secret"))
    }
}
```

Process death mid-queue is covered by the first test's shape — a fresh `Outbox` on the same `Db` and directory is what a restarted process sees — and by `SyncWorker` being WorkManager's, which re-runs it. Say so in a comment at the top of the file.

`mockwebserver3` must also be an `androidTestImplementation` in `core`.

- [ ] **Step 2: `android.yml`**

```yaml
name: android
on:
  push:
    paths: ["android/**", ".github/workflows/android.yml"]
  pull_request:
    paths: ["android/**", ".github/workflows/android.yml"]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-java@v4
        with: { distribution: temurin, java-version: "21" }
      - uses: android-actions/setup-android@v3
      - uses: gradle/actions/setup-gradle@v4
      - run: cd android && ./gradlew :core:test :app:testDebugUnitTest :app:assembleDebug --no-daemon
      - uses: actions/upload-artifact@v4
        with: { name: app-debug, path: android/app/build/outputs/apk/debug/app-debug.apk }
  device:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-java@v4
        with: { distribution: temurin, java-version: "21" }
      - uses: android-actions/setup-android@v3
      - name: enable KVM
        run: echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules && sudo udevadm control --reload-rules && sudo udevadm trigger --name-match=kvm
      - uses: reactivecircus/android-emulator-runner@v2
        with:
          api-level: 35
          arch: x86_64
          script: cd android && ./gradlew :core:connectedDebugAndroidTest :app:connectedDebugAndroidTest --no-daemon
```

Pin action versions to the ones the other workflows in `.github/workflows/` already use, if they differ. If `jvmToolchain(17)` was kept in Task 1, Java 21 on CI still satisfies it through toolchain auto-provisioning; if it was changed to 25, set `java-version: "25"`.

- [ ] **Step 3: Run what can run, push, watch CI**

Run: `cd android && ./gradlew :core:test :app:testDebugUnitTest :app:assembleDebug`. Then `git push` and `gh run watch`. Expected: both jobs green. If the emulator job is flaky on the first run, retry once before changing anything.

- [ ] **Step 4: README and commit**

Append to `android/README.md` the two CI jobs, the distributor note (ntfy or NextPush from F-Droid), and the pairing steps.

```bash
git add android .github/workflows/android.yml
git commit -m "test(android): the outbox on a device, and a CI job for the app

Evidence: ./gradlew :core:test :app:testDebugUnitTest — N passed; android.yml build and device jobs green on <run url>.

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

## Self-review against the spec

- Frame, modules, application id, Play-Services-free: Task 1. `Transport` internal: Task 4.
- Connection: Task 3 (Keystore-wrapped file in place of the deprecated `EncryptedSharedPreferences`). Clearing keeps rows: `unpair` in Task 8 does not touch the outbox; refused rows are requeued on re-pair.
- PairUri: Task 2. Transport, claim, TOFU pin, fingerprint from the URI: Task 4. The five calls: Task 4.
- Situation, stable half, place, `bundle-fields.txt`: Task 5 and the vocabulary plan.
- Outbox table, files-first copy, transitions, ladder, seven-day sweep: Task 6, with `sweepSent` called at the start of every `SyncWorker` pass in Task 7.
- Cache (`moments`): Task 6 and 8. Sync worker, unique, network-constrained, rescheduled at the rung: Task 7.
- Push registration, decode by the connector, versions, unknown rings: Tasks 8, 12, 14.
- Four screens: 9, 10, 11, 12. Doors: 13 (share, process-text, tile, shortcut, scheme in 9), camera and microphone in 11. Notification actions: 14.
- Theme from the tokens: 9. Failures: unreachable (11's words), refused (9's banner, requeue in 8), pin mismatch (9's screen), no distributor (12).
- Build, CI, instrumentation: 1 and 15. Nothing asserts about ranking.

Gaps knowingly left: `AndroidSituationSource` has no unit test (all platform reads; shown live in Settings). The microphone door is described in Task 11 rather than fully coded; implement it as described. F-Droid metadata (`fastlane/`) and release signing are not in this plan.
