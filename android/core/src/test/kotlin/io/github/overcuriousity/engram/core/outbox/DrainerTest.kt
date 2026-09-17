package io.github.overcuriousity.engram.core.outbox

import androidx.test.core.app.ApplicationProvider
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
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import kotlin.io.path.createTempDirectory

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class DrainerTest {
    private val server = MockWebServer()
    private var now = 5_000_000L
    private val db = Db.inMemory(ApplicationProvider.getApplicationContext())
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
        assertEquals(State.held, rows[0].state); assertEquals("that body is not valid UTF-8 text", rows[0].error)
        assertEquals(State.sent, rows[1].state)
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

    @Test fun aMomentTheServerNoLongerHasIsSettled() = runTest {
        box.enqueueDone("gone")
        server.enqueue(MockResponse(code = 404, body = """{"error":"no such moment"}"""))
        assertEquals(Drainer.Outcome.Done, drainer.drainOnce())
        assertEquals(State.sent, box.rows.first().single().state)
    }

    @Test fun aRefusedSnoozeIsHeldRatherThanRetriedForever() = runTest {
        box.enqueueSnooze("m1", 1L)
        server.enqueue(MockResponse(code = 400, body = """{"error":"until must be in the future"}"""))
        assertEquals(Drainer.Outcome.Done, drainer.drainOnce())
        val r = box.rows.first().single()
        assertEquals(State.held, r.state); assertEquals(400, r.status)
        assertEquals("until must be in the future", r.error)
    }

    @Test fun aRowWithNoMomentIsHeldAndTheRestGo() = runTest {
        box.enqueueDone("m1"); now += 1; box.enqueueText("good", null, null)
        // The payload of the first row, emptied behind the outbox's back: the
        // shapes a sloppy provider or an older build can leave in the table.
        val bad = box.rows.first().minBy { it.createdAt }.id
        db.outboxDao().let { dao -> dao.update(dao.get(bad)!!.copy(payload = "{}")) }
        server.enqueue(MockResponse(code = 201, body = "{}"))
        assertEquals(Drainer.Outcome.Done, drainer.drainOnce())
        val rows = box.rows.first().sortedBy { it.createdAt }
        assertEquals(State.held, rows[0].state); assertEquals("the row carries no moment", rows[0].error)
        assertEquals(State.sent, rows[1].state)
        assertEquals(1, server.requestCount)
    }

    @Test fun nothingDueIsLater() = runTest {
        box.enqueueText("a", null, null); box.failed(box.rows.first().single().id, "x")
        val out = drainer.drainOnce()
        assertTrue(out is Drainer.Outcome.Later)
        assertEquals(0, server.requestCount)
    }
}
