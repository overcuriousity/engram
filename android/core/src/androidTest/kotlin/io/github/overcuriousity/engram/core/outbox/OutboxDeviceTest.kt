package io.github.overcuriousity.engram.core.outbox

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.github.overcuriousity.engram.core.Connection
import io.github.overcuriousity.engram.core.ConnectionStore
import io.github.overcuriousity.engram.core.KeystoreBox
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.State
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

/**
 * The outbox on a real device: the thing that must not be trusted to review.
 * Process death mid-queue is covered by the first test's shape — a fresh
 * `Outbox` on the same database and directory is exactly what a restarted
 * process sees — and by `SyncWorker` being WorkManager's, which re-runs it.
 */
@RunWith(AndroidJUnit4::class)
class OutboxDeviceTest {
    private val ctx = ApplicationProvider.getApplicationContext<Context>()

    @Test fun acceptedOfflineDeliveredOnReconnectBytesIntact() = runBlocking {
        val dir = File(ctx.cacheDir, "outbox-test").apply { deleteRecursively(); mkdirs() }
        val db = Db.open(ctx)
        val box = Outbox(db, dir)
        val id = box.enqueueFiles(
            listOf(Incoming("a.bin", "application/octet-stream") { ByteArray(4096) { it.toByte() }.inputStream() }),
            null, null,
        )
        // "Offline": a transport pointed at a closed port.
        val dead = Transport(Connection("http://127.0.0.1:1", "t", null, "1", "d"), "ua")
        assertTrue(Drainer(box, dead, { "UTC" }, System::currentTimeMillis).drainOnce() is Drainer.Outcome.Later)
        assertEquals(State.queued, box.rows.first().first { it.id == id }.state)
        // Reconnect, as a fresh process would: a new Outbox on the same store.
        val server = MockWebServer().apply { start(); enqueue(MockResponse(code = 201, body = "{}")) }
        val live = Transport(Connection(server.url("/").toString().trimEnd('/'), "t", null, "1", "d"), "ua")
        val later = { System.currentTimeMillis() + 60_000 } // past the first rung
        val again = Outbox(db, dir, later)
        Drainer(again, live, { "UTC" }, later).drainOnce()
        assertEquals(State.sent, box.rows.first().first { it.id == id }.state)
        val body = server.takeRequest().body!!.toByteArray()
        assertTrue(body.size > 4096) // multipart framing plus the 4096 bytes, byte-exact inside
        val expected = ByteArray(4096) { it.toByte() }
        assertTrue(indexOf(body, expected) >= 0)
        server.close()
        db.close()
    }

    @Test fun theKeystoreBoxRoundTrips() {
        val f = File(ctx.filesDir, "conn-test").apply { delete() }
        val c = Connection("https://x", "engram_secret", "pin", "1", "d")
        ConnectionStore(f, KeystoreBox("engram-test")).set(c)
        assertEquals(c, ConnectionStore(f, KeystoreBox("engram-test")).current.value)
        assertFalse(f.readText(Charsets.ISO_8859_1).contains("engram_secret"))
    }

    private fun indexOf(hay: ByteArray, needle: ByteArray): Int {
        outer@ for (i in 0..hay.size - needle.size) {
            for (j in needle.indices) if (hay[i + j] != needle[j]) continue@outer
            return i
        }
        return -1
    }
}
