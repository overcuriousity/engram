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

    // ── Reads ────────────────────────────────────────────────────────────────

    @Test fun aReadReturnsItsTagAndSendsItBack() = runTest {
        server.enqueue(MockResponse.Builder().code(200).body("""{"items":[]}""").addHeader("ETag", "\"abc\"").build())
        server.enqueue(MockResponse(code = 304))
        val first = t.get("/api/v1/corpora", mapOf("limit" to "2", "after" to null))
        assertEquals(Got(200, """{"items":[]}""", "\"abc\""), first)
        assertEquals("/api/v1/corpora?limit=2", server.takeRequest().target)

        val again = t.get("/api/v1/corpora", mapOf("limit" to "2"), etag = first.etag)
        assertEquals(304, again.status)
        assertEquals("", again.body)
        assertEquals("\"abc\"", server.takeRequest().headers["If-None-Match"])
    }

    @Test fun aReadRefusedIsRefused() = runTest {
        server.enqueue(MockResponse(code = 401))
        try { t.get("/api/v1/status"); fail() } catch (e: Refused) {}
    }

    // ── The stream ───────────────────────────────────────────────────────────

    @Test fun framesArriveInOrderWithCommentsDroppedAndDataJoined() = runTest {
        val sse = ": keep-alive\n\n" +
            "event: token\ndata: {\"text\":\"a\"}\n\n" +
            "event: token\ndata: line one\ndata: line two\n\n" +
            "event: done\ndata: {}\n\n"
        server.enqueue(MockResponse.Builder().code(200).addHeader("Content-Type", "text/event-stream").body(sse).build())
        val seen = mutableListOf<Pair<String, String>>()
        val status = t.stream("/api/v1/ask/stream", mapOf("door" to "android"), """{"q":"why"}""") { e, d -> seen += e to d }
        assertEquals(Answer(200, ""), status)
        assertEquals(
            listOf("token" to """{"text":"a"}""", "token" to "line one\nline two", "done" to "{}"),
            seen,
        )
        val r = server.takeRequest()
        assertEquals("POST", r.method)
        assertEquals("/api/v1/ask/stream?door=android", r.target)
        assertEquals("Bearer engram_tok", r.headers["Authorization"])
        assertEquals("""{"q":"why"}""", r.body!!.utf8())
    }

    @Test fun aStreamThatIsNotOneIsItsStatusAndNoFrames() = runTest {
        server.enqueue(MockResponse(code = 502, body = """{"error":"inference[ask]: down"}"""))
        var frames = 0
        assertEquals(
            Answer(502, """{"error":"inference[ask]: down"}"""),
            t.stream("/api/v1/ask/stream", emptyMap(), "{}") { _, _ -> frames++ },
        )
        assertEquals(0, frames)
    }

    @Test fun aStreamRefusedIsRefused() = runTest {
        server.enqueue(MockResponse(code = 401))
        try { t.stream("/api/v1/ask/stream", emptyMap(), "{}") { _, _ -> }; fail() } catch (e: Refused) {}
    }
}
