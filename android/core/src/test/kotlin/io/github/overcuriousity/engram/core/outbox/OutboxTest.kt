package io.github.overcuriousity.engram.core.outbox

import androidx.test.core.app.ApplicationProvider
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.State
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.File
import kotlin.io.path.createTempDirectory

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class OutboxTest {
    private var now = 1_000_000L
    private val dir = createTempDirectory("outbox").toFile()
    private val db = Db.inMemory(ApplicationProvider.getApplicationContext())
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
