package io.github.overcuriousity.engram.core

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.github.overcuriousity.engram.core.contained.CoreFailed
import io.github.overcuriousity.engram.core.contained.CoreState
import io.github.overcuriousity.engram.core.contained.Endpoint
import io.github.overcuriousity.engram.core.contained.ModelManifest
import io.github.overcuriousity.engram.core.contained.Role
import io.github.overcuriousity.engram.core.contained.Setup
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

    private var given: Setup? = null
    private var halted = 0
    private fun engram(boot: () -> Started = { Started(theCore.port, "launch") }) =
        Engram(app, "0", PlainBox(), { _, setup -> given = setup; boot() }, { halted++ }).also { open += it }
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

    private fun install(role: Role) = ModelManifest.defaultFor(role)!!.let { m ->
        File(app.filesDir, "contained/models").apply { mkdirs() }.resolve(m.file)
            .also { f -> java.io.RandomAccessFile(f, "rw").use { it.setLength(m.bytes) } }
    }

    @Test fun aModelThatArrivesIsNamedAfterARestart() = runTest {
        choose(Mode.contained)
        val e = engram()
        assertTrue(e.ready())
        assertNull(given!!.embed)
        val f = install(Role.embed)
        assertTrue(e.installed(ModelManifest.required.single()))
        assertTrue(e.restartCore())
        assertEquals(f.path, given!!.embed)
    }

    @Test fun anEndpointReachesTheCoreOnlyWhenAskIsSetToIt() = runTest {
        choose(Mode.contained)
        val ask = install(Role.ask)
        val e = engram()
        e.askEndpoint = Endpoint("https://llm.example/v1", "a-model", "sk-1")
        assertTrue(e.ready())
        assertEquals(ask.path, given!!.ask); assertNull(given!!.askEndpoint)
        e.modes.ask = AskVia.endpoint
        assertTrue(e.restartCore())
        assertNull(given!!.ask); assertEquals("https://llm.example/v1", given!!.askEndpoint!!.baseUrl)
        e.modes.ask = AskVia.off
        assertTrue(e.restartCore())
        assertNull(given!!.ask); assertNull(given!!.askEndpoint)
    }

    @Test fun unpairingDoesNotForgetWhereToAsk() = runTest {
        val e = engram().also { it.store.set(paired()) }
        e.askEndpoint = Endpoint("https://llm.example/v1", "a-model")
        e.store.clear()
        assertEquals("a-model", e.askEndpoint!!.model)
    }

    @Test fun serverModeHasNothingToDownloadInto() = runTest {
        assertNull(engram().downloader)
        assertFalse(engram().installed(ModelManifest.required.single()))
    }

    @Test fun shuttingAnInstanceStopsItsCoreAndLeavesBothBasesWhereTheyWere() = runTest {
        choose(Mode.contained)
        val c = engram()
        assertTrue(c.ready())
        c.outbox.enqueueText("kept on the phone", null, null)
        c.shutdown()
        assertEquals(1, halted)
        assertNull(c.connection.value)

        choose(Mode.server)
        engram().also { assertTrue(it.outbox.rows.first().isEmpty()) }.shutdown()
        assertEquals("a server instance has no core to stop", 1, halted)

        choose(Mode.contained)
        assertEquals(1, engram().outbox.rows.first().size)
    }

    @Test fun whatContainedModeCannotOpenWithoutIsTheEmbedder() = runTest {
        assertTrue(engram().requiredMissing().isEmpty())
        choose(Mode.contained)
        val e = engram()
        assertEquals(ModelManifest.required, e.requiredMissing())
        install(Role.embed)
        assertTrue(e.requiredMissing().isEmpty())
    }

    @Test fun askWantsAModelOnlyWhereOneWouldBeUsed() = runTest {
        assertFalse(engram().askWantsAModel)
        choose(Mode.contained)
        val e = engram()
        assertTrue(e.askWantsAModel)
        e.modes.ask = AskVia.endpoint; assertFalse(e.askWantsAModel)
        e.modes.ask = AskVia.off; assertFalse(e.askWantsAModel)
        e.modes.ask = AskVia.device
        install(Role.ask)
        assertFalse(e.askWantsAModel)
    }

    @Test fun aPassIsWantedOnlyWhereAnEndpointWouldDoTheWork() = runTest {
        assertFalse(engram().passWanted)
        choose(Mode.contained)
        val e = engram()
        assertFalse(e.passWanted)
        e.modes.ask = AskVia.endpoint
        assertFalse("set to an endpoint that was never written down", e.passWanted)
        e.askEndpoint = Endpoint("https://llm.example/v1", "m")
        assertTrue(e.passWanted)
        e.modes.ask = AskVia.device
        assertFalse("the model on the phone answers questions and reads nothing", e.passWanted)
    }

    @Test fun whatWaitsIsReadFromTheCoresStatus() = runTest {
        choose(Mode.contained)
        val e = engram()
        assertTrue(e.ready())
        theCore.enqueue(MockResponse(code = 200, body = """{"waiting_generation": 7}"""))
        assertEquals(7, e.waitingGeneration())
        theCore.enqueue(MockResponse(code = 503))
        assertNull(e.waitingGeneration())
    }

    @Test fun aContainedPhoneWritesDownWhatIsDueAndForgetsWhatNoLongerIs() = runTest {
        choose(Mode.contained)
        val e = engram()
        assertTrue(e.ready())
        val now = 2_000_000L
        fun listing(vararg rows: String) = MockResponse(code = 200, body = """{"items":[${rows.joinToString(",")}],"next":null}""")
        fun due(id: String, at: Long) = """{"moment":{"id":"$id","artifact_id":"x","at":$at},"title":"$id","named":true}"""
        theCore.enqueue(listing(due("soon", now + 60), due("missed", now - 60)))
        val rings = io.github.overcuriousity.engram.core.reminders.LocalReminders.sync(e, now)!!
        assertEquals(listOf("missed", "soon"), rings.map { it.id })
        assertTrue(theCore.takeRequest().target.startsWith("/api/v1/moments?kind=due&to="))
        assertEquals(2, e.db.momentsDao().all().size)

        e.rung = setOf("missed", "gone-long-ago")
        theCore.enqueue(listing(due("missed", now - 60)))
        val again = io.github.overcuriousity.engram.core.reminders.LocalReminders.sync(e, now)!!
        assertTrue("a missed one that rang does not ring again", again.isEmpty())
        assertTrue(e.db.momentsDao().all().isEmpty())
        assertEquals(setOf("missed"), e.rung)
    }

    @Test fun aServersClientSetsNoAlarmsOfItsOwn() = runTest {
        assertNull(io.github.overcuriousity.engram.core.reminders.LocalReminders.sync(engram().also { it.store.set(paired()) }))
        assertEquals(0, theServer.requestCount)
    }

    @Test fun aContainedInstanceDoesNotPair() = runTest {
        choose(Mode.contained)
        val e = engram()
        val refused = runCatching { e.pair(PairUri.parse("engram://pair?o=https%3A%2F%2Fx.test&c=abc&v=0.1.0")!!) }.exceptionOrNull()
        assertTrue(refused is IllegalStateException)
        assertNull(e.store.current.value)
    }
}
