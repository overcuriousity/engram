package io.github.overcuriousity.engram.core.read

import androidx.test.core.app.ApplicationProvider
import io.github.overcuriousity.engram.core.Connection
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.ask.Ask
import io.github.overcuriousity.engram.core.ask.Phase
import io.github.overcuriousity.engram.core.db.Db
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Assume.assumeNotNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.time.LocalDate

/**
 * The phone's `core` against a real engram, over real HTTP. Skipped unless a
 * server is named:
 *
 *     ./gradlew :core:testDebugUnitTest --tests '*LiveServerTest*' \
 *         -Pengram.live.origin=http://127.0.0.1:18080 -Pengram.live.token=engram_…
 *
 * The fixtures prove the shapes; this proves the conversation — the tag that
 * comes back is the tag that earns a 304, the cursor the server hands out is
 * one it takes back. It wants a base with at least three captures in it and
 * asserts nothing about what they say or what order anything is in.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class LiveServerTest {
    private val origin: String? = System.getProperty("engram.live.origin")
    private val token: String? = System.getProperty("engram.live.token")
    private val db = Db.inMemory(ApplicationProvider.getApplicationContext())
    private var now = 1_000L

    private fun transport() = Transport(Connection(origin!!, token!!, null, "live", "live test"), "engram-android/live (test)")
    private fun reader() = ServerReader({ transport() }, db.cacheDao(), { now }, onRefused = { fail("refused") }, onPinMismatch = {})

    @Test fun theLibraryPagesAndRevalidates() = runBlocking {
        assumeNotNull(origin, token)
        val r = reader()
        val first = r.read(Request("/api/v1/corpora", mapOf("limit" to "2")), Decode.corpora).toList().last()
        assertEquals(Reach.Fresh, first.reach)
        val page = first.value!!
        assertEquals(2, page.items.size)
        assertTrue(page.items.all { it.label.isNotEmpty() })
        assertNotNull("three captures are more than one page of two", page.next)

        // The cursor the server handed out is one it takes back.
        val second = r.read(Request("/api/v1/corpora", mapOf("limit" to "2", "after" to page.next)), Decode.corpora).toList().last().value!!
        assertTrue(second.items.isNotEmpty())
        assertTrue(second.items.none { s -> page.items.any { it.id == s.id } })

        // And the tag it sent is the tag that earns a 304: same body, later time.
        now = 9_000L
        val again = r.read(Request("/api/v1/corpora", mapOf("limit" to "2")), Decode.corpora).toList()
        assertEquals(page, again.first().value)
        assertEquals(1_000L, again.first().fetchedAt)
        assertEquals(page, again.last().value)
        assertEquals(9_000L, again.last().fetchedAt)
    }

    @Test fun aCorpusItsArtifactsAndTheirHistoryAllDecode() = runBlocking {
        assumeNotNull(origin, token)
        val r = reader()
        val row = r.read(Api.corpora(null), Decode.corpora).toList().last().value!!.items.first()
        val corpus = r.read(Api.corpus(row.id), Decode.corpus).toList().last().value!!
        assertEquals(row.id, corpus.id)
        assertTrue(corpus.text.isNotEmpty())
        val chunk = corpus.chunks.firstOrNull() ?: return@runBlocking
        val artifact = r.read(Api.artifact(chunk.id), Decode.artifact).toList().last().value!!
        assertEquals(chunk.id, artifact.chunk.id)
        assertEquals(row.id, artifact.source?.id)
        assertNotNull(r.read(Api.lineage(chunk.id), Decode.lineage).toList().last().value)
        assertNotNull(r.read(Api.versions(chunk.id), Decode.versions).toList().last().value)
    }

    @Test fun todayDecodesAndAThingThatIsNotThereIsSaidInTheServersWords() = runBlocking {
        assumeNotNull(origin, token)
        val r = reader()
        val day = r.read(Api.day(LocalDate.now().toString(), "UTC"), Decode.day).toList().last()
        assertEquals(LocalDate.now().toString(), day.value!!.date)
        assertNotNull(r.read(Api.due(), Decode.due).toList().last().value)

        val gone = r.read(Api.artifact("no-such-artifact"), Decode.artifact).toList().last()
        assertNull(gone.value)
        assertEquals("not found", gone.error)
        assertEquals(Reach.Fresh, gone.reach)

        assertNotNull(r.ask(Api.CONTEXT, """{"tz":"UTC"}""", Decode.offer).value)
    }

    @Test fun anAskEndsOneWayOrTheOtherAndNeverHangs() = runBlocking {
        assumeNotNull(origin, token)
        val last = Ask({ transport() }, db.askedDao()) { now }.run("what is in here?").toList().last()
        assertTrue("ended ${last.phase}: ${last.error}", last.phase == Phase.Done || last.phase == Phase.Failed)
        if (last.phase == Phase.Failed) assertFalse(last.error.isNullOrBlank())
        println("LIVE ask ended ${last.phase}: ${last.error ?: last.text.take(80)}")
    }
}
