package io.github.overcuriousity.engram.core.read

import androidx.test.core.app.ApplicationProvider
import io.github.overcuriousity.engram.core.Connection
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Db
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class ServerReaderTest {
    private val server = MockWebServer()
    private val db = Db.inMemory(ApplicationProvider.getApplicationContext())
    private var now = 1_000L
    private var refused = 0
    private lateinit var origin: String
    private lateinit var reader: ServerReader

    private fun readerFor(origin: String) = ServerReader(
        transport = { Transport(Connection(origin, "engram_tok", null, "0.1.0", "dev"), "ua") },
        dao = db.cacheDao(),
        clock = { now },
        onRefused = { refused++ },
        onPinMismatch = {},
    )

    @Before fun up() {
        server.start()
        origin = server.url("/").toString().trimEnd('/')
        reader = readerFor(origin)
    }
    @After fun down() = server.close()

    private val req = Request("/api/v1/corpora", mapOf("limit" to "2", "after" to null))
    private fun ok(body: String, tag: String) = MockResponse.Builder().code(200).body(body).addHeader("ETag", tag).build()

    @Test fun theKeyIsThePathAndItsQueryInOneOrder() {
        assertEquals("/a?b=1&c=2", Request("/a", mapOf("c" to "2", "b" to "1", "z" to null)).key)
        assertEquals("/a", Request("/a").key)
    }

    @Test fun aFirstReadShowsNothingThenTheAnswer() = runTest {
        server.enqueue(ok("one", "\"t1\""))
        val seen = reader.read(req) { it }.toList()
        assertEquals(listOf(Read(null, null, Reach.Fresh, loading = true), Read("one", 1_000L, Reach.Fresh, loading = false)), seen)
    }

    @Test fun aSecondReadShowsWhatIsHeldAndRevalidatesIt() = runTest {
        server.enqueue(ok("one", "\"t1\""))
        reader.read(req) { it }.toList()
        server.takeRequest()
        now = 5_000L
        server.enqueue(MockResponse(code = 304))

        val seen = reader.read(req) { it }.toList()

        assertEquals("\"t1\"", server.takeRequest().headers["If-None-Match"])
        assertEquals(Read("one", 1_000L, Reach.Fresh, loading = true), seen.first())
        // Not modified: the same body, and the time it was last known to be current.
        assertEquals(Read("one", 5_000L, Reach.Fresh, loading = false), seen.last())
    }

    @Test fun aChangedAnswerReplacesWhatWasHeld() = runTest {
        server.enqueue(ok("one", "\"t1\""))
        reader.read(req) { it }.toList()
        server.enqueue(ok("two", "\"t2\""))
        assertEquals("two", reader.read(req) { it }.toList().last().value)
        server.enqueue(MockResponse(code = 304))
        reader.read(req) { it }.toList()
        repeat(2) { server.takeRequest() }
        assertEquals("\"t2\"", server.takeRequest().headers["If-None-Match"])
    }

    @Test fun aServerThatCannotBeReachedSaysSoOverWhatIsHeld() = runTest {
        server.enqueue(ok("one", "\"t1\""))
        reader.read(req) { it }.toList()
        server.close()

        val last = reader.read(req) { it }.toList().last()

        assertEquals(Read("one", 1_000L, Reach.Unreachable, loading = false), last)
    }

    @Test fun andOverNothingWhenNothingIsHeld() = runTest {
        server.close()
        assertEquals(Read<String>(null, null, Reach.Unreachable, loading = false), reader.read(req) { it }.toList().last())
    }

    @Test fun aRefusalIsNotAnOutage() = runTest {
        server.enqueue(MockResponse(code = 401))
        assertEquals(Reach.Refused, reader.read(req) { it }.toList().last().reach)
        assertEquals(1, refused)
    }

    @Test fun whatTheServerSaidNoToIsSaidInItsWords() = runTest {
        server.enqueue(MockResponse(code = 400, body = """{"error":"after: not a cursor this server issued"}"""))
        val last = reader.read(req) { it }.toList().last()
        assertEquals(Reach.Fresh, last.reach)
        assertEquals("after: not a cursor this server issued", last.error)
    }

    @Test fun aThingThatIsGoneIsNotKept() = runTest {
        server.enqueue(ok("one", "\"t1\""))
        reader.read(req) { it }.toList()
        server.enqueue(MockResponse(code = 404, body = """{"error":"not found"}"""))
        val gone = reader.read(req) { it }.toList().last()
        assertNull(gone.value)
        assertEquals("not found", gone.error)
        server.close()
        assertNull(reader.read(req) { it }.toList().last().value)
    }

    @Test fun anotherServersNotesAreNeverShown() = runTest {
        server.enqueue(ok("one", "\"t1\""))
        reader.read(req) { it }.toList()
        val elsewhere = readerFor("http://127.0.0.1:9")
        val seen = elsewhere.read(req) { it }.toList()
        assertTrue(seen.all { it.value == null })
    }

    @Test fun anAnswerThatDoesNotDecodeIsAnErrorAndNotACrash() = runTest {
        server.enqueue(ok("not json", "\"t1\""))
        val last = reader.read(req) { error("cannot decode") }.toList().last()
        assertNull(last.value)
        assertEquals("unreadable answer", last.error)
    }

    /**
     * An app update can tighten a model against a body the previous build
     * cached — a field becomes required, a string becomes an enum. Sending the
     * tag anyway got a `304` back, and the row said "fresh" with nothing in
     * it and no error: a blank screen every retry reproduced, until the
     * thirty-day prune.
     */
    @Test fun aHeldBodyThisBuildCannotReadIsDroppedAndAskedForAgainWholly() = runTest {
        server.enqueue(ok("one", "\"t1\""))
        reader.read(req) { it }.toList()
        server.takeRequest()
        server.enqueue(ok("two", "\"t2\""))

        // The decode this build would do: it refuses what was held.
        val last = reader.read(req) { if (it == "one") error("no longer readable") else it }.toList().last()

        assertNull("the tag went back out, so the server said 304", server.takeRequest().headers["If-None-Match"])
        assertEquals("two", last.value)
        assertNull(last.error)
    }

    /**
     * `body` is the whole answer and nothing caps it: `GET /corpora/{id}`
     * carries `raw_text`, which for a captured book is the book, and reading a
     * column past SQLite's CursorWindow throws out of Room. Unguarded that
     * threw out of `read` and out of the composable collecting it, on the held
     * answer and before the server was asked — so retrying died the same way.
     */
    @Test fun aCacheThatThrowsCostsTheRoundTripAndNotTheScreen() = runTest {
        val broken = object : io.github.overcuriousity.engram.core.db.CacheDao {
            override suspend fun get(key: String, origin: String) = error("CursorWindow: row too big")
            override suspend fun put(row: io.github.overcuriousity.engram.core.db.CacheRow) = error("disk full")
            override suspend fun touch(key: String, origin: String, now: Long) = error("no")
            override suspend fun delete(key: String, origin: String) = error("no")
            override suspend fun deleteBefore(before: Long) = error("no")
            override suspend fun clear() = error("no")
            override suspend fun count() = 0
        }
        val r = ServerReader(
            transport = { Transport(Connection(origin, "engram_tok", null, "0.1.0", "dev"), "ua") },
            dao = broken,
            clock = { now },
            onRefused = { refused++ },
            onPinMismatch = {},
        )
        server.enqueue(ok("one", "\"t1\""))

        val seen = r.read(req) { it }.toList()

        assertEquals("one", seen.last().value)
        assertNull(seen.last().error)
    }

    @Test fun aQuestionPutToTheServerIsNeverKept() = runTest {
        server.enqueue(MockResponse(code = 200, body = """{"offer":null}"""))
        val r = reader.ask("/api/v1/context", "{}") { it }
        assertEquals("""{"offer":null}""", r.value)
        assertEquals(0, db.cacheDao().count())
        server.close()
        assertEquals(Reach.Unreachable, reader.ask("/api/v1/context", "{}") { it }.reach)
    }
}
