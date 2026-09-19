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
        assertThrows(CoreFailed::class.java) { Core.start(dir.path, Setup()) }
        assertFalse(Core.available)
    }

    @Test fun aStartedCoreIsALoopbackConnection() = runTest {
        val c = Contained(dir, { Setup(embed = "/m/embed.gguf") }, "a phone", { d, m ->
            assertEquals(dir.path, d); assertEquals("/m/embed.gguf", m.embed)
            Started(41234, "engram_launch")
        })
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
        val c = Contained(dir, { Setup() }, "a phone", { _, _ -> starts.incrementAndGet(); Started(1, "t") })
        List(8) { async { c.ensure() } }.awaitAll()
        assertEquals(1, starts.get())
    }

    @Test fun aFailureIsSaidAndTheNextAskTriesAgain() = runTest {
        var fail = true
        val c = Contained(dir, { Setup() }, "a phone", { _, _ ->
            if (fail) throw CoreFailed("the base is locked") else Started(2, "t")
        })
        assertNull(c.ensure())
        assertEquals(CoreState.Unavailable("the base is locked"), c.state.value)
        assertNull(c.connected.value)
        fail = false
        assertNotNull(c.ensure())
    }

    @Test fun aLibraryThatWillNotLinkIsUnavailableToo() = runTest {
        val c = Contained(dir, { Setup() }, "a phone", { _, _ -> throw UnsatisfiedLinkError("no engram_android") })
        assertNull(c.ensure())
        assertTrue(c.state.value is CoreState.Unavailable)
    }

    @Test fun aRestartIsANewCoreOverWhatIsThereNow() = runTest {
        var asked = Setup()
        var ask: String? = null
        val log = mutableListOf<String>()
        val c = Contained(dir, { Setup(ask = ask) }, "a phone", { _, s -> asked = s; log += "start"; Started(log.size, "t${log.size}") }, { log += "stop" })
        val first = c.ensure()!!
        ask = "/m/ask.gguf"
        val second = c.restart()!!
        assertEquals(listOf("start", "stop", "start"), log)
        assertEquals("/m/ask.gguf", asked.ask)
        assertNotEquals(first.token, second.token)
        assertEquals(second, c.connected.value)
    }
}
