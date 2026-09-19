# Contained mode, part 4 — the app side — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The app can run over the engram inside its own process: a stored mode, a loopback connection that lives only in memory, separate state per mode, and the choice made in `Engram.kt` and nowhere else.

**Architecture:** `Engram` reads the mode once, when it is built, and derives everything that differs from it: the Room file, the outbox directory, where `transport()` gets its `Connection`. In contained mode a small holder, `Contained`, calls `Core.start` on demand and keeps the resulting connection in a `StateFlow`; nothing writes it down. Screens, the reader, ask and the drainer are untouched because they already go through `transport()`.

**Tech Stack:** Kotlin, Room, WorkManager, OkHttp, Robolectric and MockWebServer for the JVM tests. No Rust changes.

**Spec:** `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md`, sections 1, 5 and 6. Task 1 corrects it.

## Global Constraints

- All commands run from `android/`. Tests: `./gradlew :core:testDebugUnitTest --tests '<pattern>'`. Baseline before this part: 236 tests, 5 skipped, 0 failures across `:core` and `:app`.
- Nothing in this part offers contained mode to a person. With no mode stored the app behaves exactly as it does today. The chooser and Settings "Mode" are part 5.
- The mode is read once per process, in `Engram`'s constructor. Nothing outside `Engram.kt` branches on it except through a property `Engram` exposes.
- The loopback `Connection` is never passed to `ConnectionStore.set`.
- Server mode's state stays where it is today (`engram.db`, `files/outbox`, `files/connection`). It is not moved.
- The `.so` exists for `arm64-v8a` only. A failed `System.loadLibrary` is "contained mode unavailable" and never a crash.
- UI copy on engram's pages is a term and a short gloss, never an explanatory sentence.
- Commit messages in the repository's style, ending with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`. No push.
- No device testing in this part. The risk, stated once: nothing here has run on a phone; the network security config, memory tagging and the library load are first exercised in part 7.

## What reading the code found

These are the places where the spec, or the brief, did not match the tree. Task 1 writes the first four into the spec.

1. **The port changes every launch, and the cache is keyed by origin.** `contained::start` binds `127.0.0.1:0`. `CacheRow` and `AskedRow` are keyed by `connection.origin`, so in contained mode every held read and every kept answer would be orphaned at each start. `Transport` gains a `source` that the cache keys by: the origin for a server, the constant `contained` for the core.
2. **The drainer's worker demands a network.** `Sync` constrains its work to `NetworkType.CONNECTED`. A contained phone in aeroplane mode would never deliver a capture to its own process. The constraint becomes the mode's.
3. **A share arrives with no activity in front.** The spec starts the core "when the app comes to the foreground", but `ShareActivity` enqueues and finishes, and `SyncWorker` then runs with no core. The core is started on demand by whoever needs it first, the worker included. When it stops is the lifecycle question and stays with part 6.
4. **Server mode keeps its paths.** The spec gives each mode "its own directory". Moving an existing install's outbox to honour that would move the one thing on the phone that exists nowhere else. Server mode stays where it is; contained mode gets `files/contained/` and a Room file of its own, `contained.db`. The core's base is under `files/contained/core/` (`control.db`, and the base under `bases/` in a file named for the tenant), not `contained/engram.db`.
5. **`Core`'s `init` block makes a failed load expensive to tell apart.** The first touch throws `ExceptionInInitializerError`, every later one `NoClassDefFoundError`. The load moves into a lazy `available`, and `start` throws `CoreFailed` when it is false.
6. **Models.** There is no downloader until part 5, so this part looks for `files/contained/models/{embed,rerank,ask}.gguf` and passes what exists. Part 5 replaces the lookup with the manifest.

## File structure

- Create `core/.../core/Mode.kt` — `Mode`, `ModeStore`, `ModeState`: what is stored, and where each mode keeps its things.
- Create `core/.../core/contained/Contained.kt` — starts the core once, holds the connection in memory, reports why it could not.
- Modify `core/.../core/contained/Core.kt` — lazy library load.
- Modify `core/.../core/Transport.kt`, `read/ServerReader.kt`, `ask/Ask.kt` — `source`.
- Modify `core/.../core/db/Db.kt` — `open(context, name)`.
- Modify `core/.../core/Engram.kt` — the choice.
- Modify `core/.../core/sync/SyncWorker.kt` — wait for the core; the constraint follows the mode.
- Modify `app/.../ui/Nav.kt`, `app/.../doors/ShareActivity.kt` — gate on `engram.connection`, not on the store.
- Create `app/src/main/res/xml/network_security_config.xml`; modify `app/src/main/AndroidManifest.xml`.
- Tests: `core/src/test/.../core/ModeTest.kt`, `contained/ContainedTest.kt`, `EngramModesTest.kt`, `sync/SyncTest.kt`; one test added to `read/ServerReaderTest.kt`.

Paths below abbreviate `android/core/src/main/kotlin/io/github/overcuriousity/engram/core` as `core/…` and the matching test root as `coretest/…`.

---

### Task 1: The spec, corrected

**Files:**
- Modify: `docs/superpowers/specs/2026-09-18-android-contained-mode-design.md` (sections 1 and 3)

- [ ] **Step 1: Section 1.** Replace the paragraph beginning "Each mode has its own directory" with:

```markdown
Each mode keeps its own state. Server mode's stays where it has always been
(`engram.db`, `files/outbox`, `files/connection`): an existing install's outbox
is the one thing on the phone that exists nowhere else, and it is not moved to
tidy a directory. Contained mode has `files/contained/`: its outbox, its
models, and the core's own directory, `core/`, holding `control.db` and the
base under `bases/`, in a file named for the tenant. Its read cache and its outbox rows are a Room file of
their own, `contained.db`. An owed write in one mode's outbox is never drained
into the other source. Switching back finds everything where it was left.

The core listens on a port of the kernel's choosing, so the origin differs at
every launch. What the app keeps per source — held reads, kept answers — is
keyed by the word `contained`, not by that origin.
```

- [ ] **Step 2: Section 3.** Replace its first paragraph's first two sentences ("`Core.start` runs when … foreground service.") with:

```markdown
`Core.start` runs when something first needs the core: the app coming to the
foreground, or the drain worker after a share with no activity in front. There
is no permanent foreground service. The drain worker asks for no network in
contained mode, since loopback needs none.
```

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-09-18-android-contained-mode-design.md docs/superpowers/plans/2026-09-19-contained-4-app-side.md
git commit -m "docs(android): the plan for contained mode, part 4, and the spec corrected by it"
```

---

### Task 2: The mode, stored, and where each mode keeps its things

**Files:**
- Create: `core/Mode.kt`
- Test: `coretest/ModeTest.kt`

**Interfaces:**
- Produces: `enum class Mode { server, contained }`; `class ModeStore(prefs: SharedPreferences) { var chosen: Mode? }`; `class ModeState(val dbName: String, val outbox: File, val core: File?, val models: File?) { fun models(): Models; companion { fun of(mode: Mode, filesDir: File): ModeState } }`.

- [ ] **Step 1: Write the failing test**

```kotlin
package io.github.overcuriousity.engram.core

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.File
import kotlin.io.path.createTempDirectory

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class ModeTest {
    private val prefs = ApplicationProvider.getApplicationContext<Context>().getSharedPreferences("m", Context.MODE_PRIVATE)
    private val files = createTempDirectory("files").toFile()

    @Test fun nothingIsChosenUntilSomethingIs() {
        assertNull(ModeStore(prefs).chosen)
        ModeStore(prefs).chosen = Mode.contained
        assertEquals(Mode.contained, ModeStore(prefs).chosen)
    }

    @Test fun aWordThisBuildDoesNotKnowIsNoChoice() {
        prefs.edit().putString("mode", "orbital").apply()
        assertNull(ModeStore(prefs).chosen)
    }

    @Test fun serverModeStaysWhereItAlwaysWas() {
        val s = ModeState.of(Mode.server, files)
        assertEquals("engram.db", s.dbName)
        assertEquals(File(files, "outbox"), s.outbox)
        assertNull(s.core)
    }

    @Test fun containedModeSharesNoPathWithIt() {
        val s = ModeState.of(Mode.contained, files)
        assertEquals("contained.db", s.dbName)
        assertEquals(File(files, "contained/outbox"), s.outbox)
        assertEquals(File(files, "contained/core"), s.core)
    }

    @Test fun onlyModelFilesThatExistAreNamed() {
        val s = ModeState.of(Mode.contained, files)
        assertEquals(io.github.overcuriousity.engram.core.contained.Models(), s.models())
        s.models!!.mkdirs()
        File(s.models, "embed.gguf").writeText("x")
        assertEquals(File(s.models, "embed.gguf").path, s.models().embed)
        assertNull(s.models().ask)
    }
}
```

- [ ] **Step 2: Run it** — `./gradlew :core:testDebugUnitTest --tests '*ModeTest*'`. Expected: compilation fails, `Mode` unresolved.

- [ ] **Step 3: Implement `core/Mode.kt`**

```kotlin
package io.github.overcuriousity.engram.core

import android.content.SharedPreferences
import io.github.overcuriousity.engram.core.contained.Models
import java.io.File

/** Where the engram this app shows lives: on a server it is paired with, or in this process. */
enum class Mode { server, contained }

/** The one stored value. Null until somebody chooses, and an install that never chose is a server's client. */
class ModeStore(private val prefs: SharedPreferences) {
    var chosen: Mode?
        get() = prefs.getString("mode", null)?.let { w -> Mode.entries.firstOrNull { it.name == w } }
        set(v) = prefs.edit().putString("mode", v?.name).apply()
}

/**
 * Everything a mode keeps on this phone. The two share no path, which is the
 * whole of how one mode's outbox is never drained into the other.
 *
 * Server mode's places are the ones it had before there were modes: its outbox
 * exists nowhere else, and is not moved to tidy a directory.
 */
class ModeState(val dbName: String, val outbox: File, val core: File?, val models: File?) {
    /** The model file for each role, where there is one. Part 5's manifest replaces the names. */
    fun models(): Models {
        fun at(name: String) = models?.let { File(it, name) }?.takeIf { it.isFile }?.path
        return Models(embed = at("embed.gguf"), rerank = at("rerank.gguf"), ask = at("ask.gguf"))
    }

    companion object {
        fun of(mode: Mode, filesDir: File): ModeState = when (mode) {
            Mode.server -> ModeState("engram.db", File(filesDir, "outbox"), core = null, models = null)
            Mode.contained -> File(filesDir, "contained").let {
                ModeState("contained.db", File(it, "outbox"), File(it, "core"), File(it, "models"))
            }
        }
    }
}
```

- [ ] **Step 4: Run it again.** Expected: 5 tests pass.

- [ ] **Step 5: Commit** — `git add` the two files; message `feat(android): a stored mode, and a place for each mode's things`.

---

### Task 3: A core that may not be there, started once, held in memory

**Files:**
- Modify: `core/contained/Core.kt`
- Create: `core/contained/Contained.kt`
- Test: `coretest/contained/ContainedTest.kt`

**Interfaces:**
- Produces: `Core.available: Boolean`; `sealed interface CoreState { Idle, Starting, Running(connection), Unavailable(why) }`; `class Contained(dataDir: File, models: () -> Models, deviceName: String, boot: (String, Models) -> Started = Core::start) { val state: StateFlow<CoreState>; val connected: StateFlow<Connection?>; suspend fun ensure(): Connection? }`.

- [ ] **Step 1: Write the failing test**

```kotlin
package io.github.overcuriousity.engram.core.contained

import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.atomic.AtomicInteger
import kotlin.io.path.createTempDirectory

class ContainedTest {
    private val dir = createTempDirectory("core").toFile()

    @Test fun onThisMachineTheLibraryIsAbsentAndThatIsAnAnswerNotACrash() {
        // The .so is arm64-v8a only; a desktop JVM is the x86_64 emulator's case.
        assertFalse(Core.available)
        assertThrows(CoreFailed::class.java) { Core.start(dir.path, Models()) }
        assertFalse(Core.available)
    }

    @Test fun aStartedCoreIsALoopbackConnection() = runTest {
        val c = Contained(dir, { Models(embed = "/m/embed.gguf") }, "a phone") { d, m ->
            assertEquals(dir.path, d); assertEquals("/m/embed.gguf", m.embed)
            Started(41234, "engram_launch")
        }
        assertEquals(CoreState.Idle, c.state.value)
        val conn = c.ensure()!!
        assertEquals("http://127.0.0.1:41234", conn.origin)
        assertEquals("engram_launch", conn.token)
        assertNull(conn.pin)
        assertEquals(conn, c.connected.value)
        assertEquals(CoreState.Running(conn), c.state.value)
    }

    @Test fun itIsStartedOnceHoweverManyAsk() = runTest {
        val starts = AtomicInteger()
        val c = Contained(dir, { Models() }, "a phone") { _, _ -> starts.incrementAndGet(); Started(1, "t") }
        List(8) { async { c.ensure() } }.awaitAll()
        assertEquals(1, starts.get())
    }

    @Test fun aFailureIsSaidAndTheNextAskTriesAgain() = runTest {
        var fail = true
        val c = Contained(dir, { Models() }, "a phone") { _, _ ->
            if (fail) throw CoreFailed("the base is locked") else Started(2, "t")
        }
        assertNull(c.ensure())
        assertEquals(CoreState.Unavailable("the base is locked"), c.state.value)
        assertNull(c.connected.value)
        fail = false
        assertNotNull(c.ensure())
    }

    @Test fun aLibraryThatWillNotLinkIsUnavailableToo() = runTest {
        val c = Contained(dir, { Models() }, "a phone") { _, _ -> throw UnsatisfiedLinkError("no engram_android") }
        assertNull(c.ensure())
        assertTrue(c.state.value is CoreState.Unavailable)
    }
}
```

- [ ] **Step 2: Run it** — `./gradlew :core:testDebugUnitTest --tests '*ContainedTest*'`. Expected: does not compile (`Contained`, `Core.available`).

- [ ] **Step 3: `Core.kt`.** Delete the `init { System.loadLibrary("engram_android") }` block and put in its place:

```kotlin
    /**
     * Whether this build carries the core for this device. The library is
     * built for arm64-v8a alone, so on anything else — an x86_64 emulator, a
     * checkout built without Rust — contained mode is absent rather than
     * broken. Asked once; a load that failed is not going to succeed later.
     */
    val available: Boolean by lazy { runCatching { System.loadLibrary("engram_android") }.isSuccess }
```

and make the first line of `fun start(dataDir: String, models: Models): Started`:

```kotlin
        if (!available) throw CoreFailed("not built for this device")
```

and the body of `shutdown()`:

```kotlin
        if (available) stop()
```

- [ ] **Step 4: `Contained.kt`**

```kotlin
package io.github.overcuriousity.engram.core.contained

import io.github.overcuriousity.engram.core.Connection
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.io.File

sealed interface CoreState {
    data object Idle : CoreState
    data object Starting : CoreState
    data class Running(val connection: Connection) : CoreState
    data class Unavailable(val why: String) : CoreState
}

/**
 * The core, started when something first needs it, and the connection to it.
 *
 * That connection is a port and a token good for this launch. It lives here,
 * in memory, and is never handed to `ConnectionStore`: written down, it would
 * be a dead origin and a revoked token by the next start.
 */
class Contained(
    private val dataDir: File,
    private val models: () -> Models,
    private val deviceName: String,
    private val boot: (String, Models) -> Started = Core::start,
) {
    private val gate = Mutex()
    private val _state = MutableStateFlow<CoreState>(CoreState.Idle)
    val state: StateFlow<CoreState> get() = _state
    private val _connected = MutableStateFlow<Connection?>(null)
    val connected: StateFlow<Connection?> get() = _connected

    /** The connection, starting the core if it is not running. Null where it cannot be; [state] says why. */
    suspend fun ensure(): Connection? = gate.withLock {
        _connected.value?.let { return it }
        _state.value = CoreState.Starting
        // Throwable, not Exception: a library that does not link is an Error.
        val started = withContext(Dispatchers.IO) { runCatching { boot(dataDir.path, models()) } }
        started.fold(
            onSuccess = {
                val c = Connection("http://127.0.0.1:${it.port}", it.token, pin = null, serverVersion = "", deviceName = deviceName)
                _connected.value = c
                _state.value = CoreState.Running(c)
                c
            },
            onFailure = {
                _state.value = CoreState.Unavailable(it.message ?: it.javaClass.simpleName)
                null
            },
        )
    }
}
```

- [ ] **Step 5: Run it again.** Expected: 5 pass.

- [ ] **Step 6: Commit** — message `feat(android): the core started on demand, and absent rather than fatal where it was not built`.

---

### Task 4: What is kept per source is keyed by the source

**Files:**
- Modify: `core/Transport.kt` (constructor), `core/read/ServerReader.kt:41`, `core/ask/Ask.kt` (three reads of `connection.origin`)
- Test: `coretest/read/ServerReaderTest.kt` (one test added)

**Interfaces:**
- Produces: `Transport(connection, userAgent, client = null, source: String = connection.origin)` with `val source`.

- [ ] **Step 1: Write the failing test.** Read the top of `ServerReaderTest.kt` first and reuse its fixture names; the test, in its terms:

```kotlin
    @Test fun aSourceThatMovesPortKeepsWhatItHeld() = runTest {
        // The contained core listens somewhere new at every launch.
        val first = MockWebServer().apply { start() }
        val second = MockWebServer().apply { start() }
        fun reader(s: MockWebServer) = ServerReader(
            { Transport(Connection(s.url("/").toString().trimEnd('/'), "t", null, "", "d"), "ua", source = "contained") },
            db.cacheDao(), { 1_000L }, {}, {},
        )
        first.enqueue(MockResponse.Builder().code(200).addHeader("ETag", "\"a\"").body("""{"v":1}""").build())
        reader(first).read(Request("k", "/api/v1/x")) { it }.toList()
        second.enqueue(MockResponse(code = 304))
        val reads = reader(second).read(Request("k", "/api/v1/x")) { it }.toList()
        assertEquals("""{"v":1}""", reads.first().value)
        assertEquals("\"a\"", second.takeRequest().headers["If-None-Match"])
        first.close(); second.close()
    }
```

(`Request`'s constructor and `db` are whatever that file already uses; match them.)

- [ ] **Step 2: Run it** — `--tests '*ServerReaderTest*'`. Expected: does not compile, no parameter `source`.

- [ ] **Step 3: Implement.** In `Transport`:

```kotlin
internal class Transport(
    val connection: Connection,
    val userAgent: String,
    client: OkHttpClient? = null,
    /**
     * What this phone files under when it keeps something from here. A
     * server's origin is its name. The core in this process listens on a new
     * port at every launch, so its origin names nothing, and it says so.
     */
    val source: String = connection.origin,
) {
```

In `ServerReader.read`: `val origin = t.source`. In `Ask`: `t.connection.origin` → `t.source`, and both `transport()?.connection?.origin` → `transport()?.source`.

- [ ] **Step 4: Run** `--tests '*ServerReaderTest*' --tests '*AskTest*' --tests '*TransportTest*'`. Expected: all pass.

- [ ] **Step 5: Commit** — message `fix(android): what is held is filed under the source, not under a port`.

---

### Task 5: The choice, in `Engram.kt`

**Files:**
- Modify: `core/db/Db.kt` (`open`), `core/Engram.kt`
- Test: `coretest/EngramModesTest.kt`

**Interfaces:**
- Consumes: `ModeStore`, `ModeState`, `Contained`, `Transport(source=)`.
- Produces on `Engram`: `val modes: ModeStore`; `val mode: Mode`; `val connection: StateFlow<Connection?>`; `val core: StateFlow<CoreState>?`; `val loopback: Boolean`; `suspend fun ready(): Boolean`; `internal fun close()`; `internal constructor(app, versionName, box: SecretBox = KeystoreBox(), boot: (String, Models) -> Started = Core::start)`.

- [ ] **Step 1: Write the failing test**

```kotlin
package io.github.overcuriousity.engram.core

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.github.overcuriousity.engram.core.contained.CoreFailed
import io.github.overcuriousity.engram.core.contained.CoreState
import io.github.overcuriousity.engram.core.contained.Started
import io.github.overcuriousity.engram.core.db.State
import io.github.overcuriousity.engram.core.outbox.Drainer
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.After
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.File
import java.net.InetAddress

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class EngramModesTest {
    private val app = ApplicationProvider.getApplicationContext<Context>()
    private val theServer = MockWebServer().apply { start(InetAddress.getByName("127.0.0.1"), 0) }
    private val theCore = MockWebServer().apply { start(InetAddress.getByName("127.0.0.1"), 0) }
    private val open = mutableListOf<Engram>()

    private fun engram(boot: () -> Started = { Started(theCore.port, "launch") }) =
        Engram(app, "0", PlainBox()) { _, _ -> boot() }.also { open += it }
    private fun choose(m: Mode?) { ModeStore(app.getSharedPreferences("engram", Context.MODE_PRIVATE)).chosen = m }
    private fun paired() = Connection(theServer.url("/").toString().trimEnd('/'), "tok", null, "1", "d")

    @After fun down() { open.forEach { it.close() }; theServer.close(); theCore.close() }

    @Test fun anInstallThatNeverChoseIsAServersClient() = runTest {
        val e = engram { fail("the core was started"); Started(0, "") }
        assertEquals(Mode.server, e.mode)
        assertNull(e.core)
        assertFalse(e.loopback)
        assertFalse(e.ready())
        e.store.set(paired())
        assertTrue(e.ready())
        assertEquals(paired(), e.connection.value)
    }

    @Test fun containedIsALoopbackConnectionThatIsNeverWrittenDown() = runTest {
        choose(Mode.contained)
        val e = engram()
        assertNull(e.connection.value)
        assertTrue(e.ready())
        assertEquals("http://127.0.0.1:${theCore.port}", e.connection.value!!.origin)
        assertTrue(e.loopback)
        assertNull(e.store.current.value)
        assertFalse(File(app.filesDir, "connection").exists())
        assertEquals("contained", e.transport()!!.source)
    }

    @Test fun aCoreThatCannotStartLeavesTheAppStandingAndSaysWhy() = runTest {
        choose(Mode.contained)
        val e = engram { throw CoreFailed("not built for this device") }
        assertFalse(e.ready())
        assertNull(e.transport())
        assertNull(e.drainer())
        assertEquals(CoreState.Unavailable("not built for this device"), e.core!!.value)
    }

    @Test fun aStoredPairingDoesNotLeakIntoContainedMode() = runTest {
        engram().also { it.store.set(paired()) }.close()
        choose(Mode.contained)
        val e = engram()
        assertTrue(e.ready())
        assertEquals("http://127.0.0.1:${theCore.port}", e.transport()!!.connection.origin)
        // Still stored, for the day the phone goes back.
        assertEquals(paired(), e.store.current.value)
    }

    @Test fun oneModesOutboxIsNeverDrainedIntoTheOther() = runTest {
        // Owed to the server, and not delivered before the mode changes.
        val s = engram().also { it.store.set(paired()) }
        s.outbox.enqueueText("for the server", null, null)
        s.close()

        choose(Mode.contained)
        val c = engram()
        assertTrue(c.ready())
        assertTrue(c.outbox.rows.first().isEmpty())
        assertEquals(Drainer.Outcome.Done, c.drainer()!!.drainOnce())
        assertEquals(0, theCore.requestCount)
        theCore.enqueue(MockResponse(code = 202, body = "{}"))
        c.outbox.enqueueText("for the phone", null, null)
        c.drainer()!!.drainOnce()
        assertEquals(1, theCore.requestCount)
        assertEquals(0, theServer.requestCount)
        c.close()

        // Back again: what was owed is still owed, to the one it was owed to.
        choose(Mode.server)
        val back = engram()
        val owed = back.outbox.rows.first().single()
        assertEquals(State.queued, owed.state)
        theServer.enqueue(MockResponse(code = 202, body = "{}"))
        back.drainer()!!.drainOnce()
        assertTrue(theServer.takeRequest().body!!.utf8().contains("for the server"))
        assertEquals(1, theCore.requestCount)
    }

    @Test fun eachModesFilesAreItsOwn() = runTest {
        engram().also { it.outbox.enqueueText("x", null, null) }.close()
        choose(Mode.contained)
        engram().also { it.outbox.enqueueText("y", null, null) }.close()
        assertTrue(app.getDatabasePath("engram.db").exists())
        assertTrue(app.getDatabasePath("contained.db").exists())
    }
}
```

- [ ] **Step 2: Run it** — `--tests '*EngramModesTest*'`. Expected: does not compile.

- [ ] **Step 3: `Db.open`**

```kotlin
        /** [name] is the mode's: each keeps its outbox and its cache in a file of its own. */
        fun open(context: Context, name: String = "engram.db"): Db =
            Room.databaseBuilder(context, Db::class.java, name)
```

- [ ] **Step 4: `Engram.kt`.** The constructor and the fields it builds, replacing lines 23–30 and 60, 80–84 as they stand (everything not shown stays):

```kotlin
class Engram internal constructor(
    val app: Context,
    versionName: String,
    box: SecretBox = KeystoreBox(),
    boot: (String, Models) -> Started = Core::start,
) {
    val userAgent = userAgent(versionName, Build.MODEL)
    val deviceName = "engram for Android $versionName · ${Build.MODEL}"
    private val prefs = app.getSharedPreferences("engram", Context.MODE_PRIVATE)
    val store = ConnectionStore(File(app.filesDir, "connection"), box)

    /**
     * Where this app's engram lives, read once: everything below is built for
     * one mode, and changing it is a new process. Nothing chosen is `server`,
     * so a phone that was paired before there was a choice carries on as it was.
     *
     * This and [transport] are the only places that know. Above here there is
     * a connection or there is not.
     */
    val modes = ModeStore(prefs)
    val mode: Mode = modes.chosen ?: Mode.server
    private val state = ModeState.of(mode, app.filesDir)
    private val contained: Contained? =
        if (mode == Mode.contained) Contained(state.core!!, state::models, deviceName, boot) else null

    val db = Db.open(app, state.dbName)
    val outbox = Outbox(db, state.outbox)
    val push = Push(store, { transport() }, db)

    /** What the app is talking to, if anything. A pairing in server mode; the running core in contained. */
    val connection: StateFlow<Connection?> = contained?.connected ?: store.current

    /** The core's own story, for the screen that waits on it. Null in server mode. */
    val core: StateFlow<CoreState>? = contained?.state

    /** True where the source is this process: nothing it is owed waits for a network. */
    val loopback: Boolean get() = contained != null

    /**
     * Whether there is something to talk to, starting the core if that is what
     * it takes. The worker asks before draining and the first screen asks
     * before drawing; whoever is first pays for the start.
     */
    suspend fun ready(): Boolean = (contained?.ensure() ?: store.current.value) != null

    internal fun transport(): Transport? = when (contained) {
        null -> store.current.value?.let { Transport(it, userAgent) }
        else -> contained.connected.value?.let { Transport(it, userAgent, source = "contained") }
    }

    internal fun close() = db.close()
```

Delete the old `private val prefs` line further down (it has moved up), keep `counters`, `situation` and the rest as they are. In the companion, `Engram(ctx, v)` still compiles. Add the imports: `contained.Contained`, `contained.Core`, `contained.CoreState`, `contained.Models`, `contained.Started`, `kotlinx.coroutines.flow.StateFlow`.

- [ ] **Step 5: Run** `--tests '*EngramModesTest*'`. Expected: 6 pass. If Robolectric cannot build something `Engram` constructs eagerly, the failure names it; make that one field `by lazy` and say so in the commit.

- [ ] **Step 6: Commit** — message `feat(android): the choice between a server and the core in this process, made in one place`.

---

### Task 6: The worker waits for the core and asks for no network it does not need

**Files:**
- Modify: `core/sync/SyncWorker.kt`
- Test: `coretest/sync/SyncTest.kt`

- [ ] **Step 1: Write the failing test**

```kotlin
package io.github.overcuriousity.engram.core.sync

import androidx.work.NetworkType
import org.junit.Assert.assertEquals
import org.junit.Test

class SyncTest {
    @Test fun aServerIsOwedOverANetwork() =
        assertEquals(NetworkType.CONNECTED, Sync.constraints(loopback = false).requiredNetworkType)

    @Test fun theCoreInThisProcessIsOwedInAeroplaneModeToo() =
        assertEquals(NetworkType.NOT_REQUIRED, Sync.constraints(loopback = true).requiredNetworkType)
}
```

- [ ] **Step 2: Run it** — `--tests '*SyncTest*'`. Expected: does not compile.

- [ ] **Step 3: Implement.** In `Sync`, replace `private val online = …` with:

```kotlin
    /** A server is reached over a network. The core in this process is not, and must not wait for one. */
    internal fun constraints(loopback: Boolean): Constraints =
        Constraints.Builder().setRequiredNetworkType(if (loopback) NetworkType.NOT_REQUIRED else NetworkType.CONNECTED).build()

    private fun constraints(context: Context) = constraints(Engram.get(context).loopback)
```

and use `.setConstraints(constraints(context))` in `kick` and `scheduleAt`. In `SyncWorker.doWork`, before `engram.drainer()`:

```kotlin
        // In contained mode this may be the first thing to need the core: a
        // share is an outbox row and a kick, with no activity in front.
        if (!engram.ready()) return Result.success()
```

- [ ] **Step 4: Run** `--tests '*SyncTest*'`, then `./gradlew :core:compileDebugAndroidTestKotlin`. Expected: pass, compiles.

- [ ] **Step 5: Commit** — message `fix(android): a capture owed to this process is delivered without a network`.

---

### Task 7: The doors stop asking the store

**Files:**
- Modify: `app/.../ui/Nav.kt:118-130`, `app/.../doors/ShareActivity.kt:23`

- [ ] **Step 1: `Nav.kt`.** `val connection by engram.connection.collectAsStateWithLifecycle()`, and replace the `if (connection == null)` block with:

```kotlin
    if (connection == null) {
        val core = engram.core
        if (core == null) PairScreen(engram, initialText = pairText) else CoreStarting(engram, core)
        return
    }
```

and, beside `PinMismatchScreen`:

```kotlin
/** Contained mode before the core answers: a word while it starts, and the reason where it cannot. */
@Composable
fun CoreStarting(engram: Engram, core: StateFlow<CoreState>) {
    val state by core.collectAsStateWithLifecycle()
    LaunchedEffect(Unit) { engram.ready() }
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.Center) {
        when (val s = state) {
            is CoreState.Unavailable -> {
                Text("On this phone · unavailable", style = MaterialTheme.typography.titleLarge)
                Spacer(Modifier.height(12.dp))
                Text(s.why, style = MaterialTheme.typography.labelMedium)
            }
            else -> Text("Starting", style = MaterialTheme.typography.titleLarge)
        }
    }
}
```

Imports: `io.github.overcuriousity.engram.core.contained.CoreState`, `kotlinx.coroutines.flow.StateFlow`.

- [ ] **Step 2: `ShareActivity.kt`.** The gate becomes `if (!engram.loopback && engram.store.current.value == null)`. In contained mode a share is kept at once, and the worker starts the core to deliver it.

- [ ] **Step 3: Run** `./gradlew :app:testDebugUnitTest :app:lintDebug`. Expected: passes as before.

- [ ] **Step 4: Commit** — message `feat(android): the first screen waits for the core instead of asking to be paired`.

`SettingsScreen` still reads `engram.store.current` and shows dashes in contained mode. Its "Server" section becomes "Mode" in part 5, which is where that is put right.

---

### Task 8: Cleartext to loopback alone, and memory tagging

**Files:**
- Create: `app/src/main/res/xml/network_security_config.xml`
- Modify: `app/src/main/AndroidManifest.xml` (`<application>`)

- [ ] **Step 1: The config**

```xml
<?xml version="1.0" encoding="utf-8"?>
<!-- The core in this process is reached over plain HTTP on loopback, where
     there is nobody to eavesdrop and no name to certify. Nothing else is. -->
<network-security-config>
    <base-config cleartextTrafficPermitted="false" />
    <domain-config cleartextTrafficPermitted="true">
        <domain includeSubdomains="false">127.0.0.1</domain>
    </domain-config>
</network-security-config>
```

- [ ] **Step 2: The manifest.** On `<application>` add `android:networkSecurityConfig="@xml/network_security_config"` and `android:memtagMode="sync"`.

- [ ] **Step 3: Verify** — `./gradlew :app:assembleDebug :app:lintDebug`, then

```bash
$HOME/Android/Sdk/build-tools/*/aapt2 dump xmltree app/build/outputs/apk/debug/app-debug.apk --file AndroidManifest.xml | grep -i 'memtag\|networkSecurity'
```

Expected: both attributes present; lint clean. What this does not show is that Android honours them, which is part 7's.

- [ ] **Step 4: Commit** — message `feat(android): cleartext to loopback and nowhere else, and memory tagging asked for`.

---

### Task 9: The whole thing, checked

- [ ] **Step 1:** `./gradlew :core:testDebugUnitTest :app:testDebugUnitTest :app:lintDebug` in the background; report the counted totals against the 236/5/0 baseline, and the exit code.
- [ ] **Step 2: The client against the real core, on the desktop.** Build and run `examples/contained` with `ENGRAM_TEST_MODELS` models, seed three captures through `POST /api/v1/corpora`, run `LiveServerTest` against its `origin=` and `token=`, then search through the API and confirm each capture's status is `ready` or `partial` — that test does not look. Stop the runner with `pgrep -x contained | xargs -r kill`.
- [ ] **Step 3:** Update the memory note `android-contained-mode-design`: part 4 done, the six findings, what part 5 inherits (the chooser sets `modes.chosen` and restarts the process; `Settings` still reads the store; the model lookup is by file name).
