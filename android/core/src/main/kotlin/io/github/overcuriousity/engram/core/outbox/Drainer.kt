package io.github.overcuriousity.engram.core.outbox

import io.github.overcuriousity.engram.core.Answer
import io.github.overcuriousity.engram.core.OutFile
import io.github.overcuriousity.engram.core.PinMismatch
import io.github.overcuriousity.engram.core.Refused
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.OutboxRow
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonNull
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
        for (row in outbox.dueQueued(clock())) {
            try {
                deliver(row)
            } catch (e: Refused) {
                outbox.refuseAll()
                return Outcome.Refused
            } catch (e: PinMismatch) {
                return Outcome.Pinned(e)
            } catch (e: IOException) {
                // A failure does not stop the pass: the row moves out on its
                // rung and the next one gets its turn.
                outbox.failed(row.id, e.message ?: e.javaClass.simpleName)
            }
        }
        val pending = outbox.dueQueued(Long.MAX_VALUE).minOfOrNull { it.nextAt }
        return if (pending == null) Outcome.Done else Outcome.Later(pending)
    }

    private suspend fun deliver(row: OutboxRow) {
        val p = Json.parseToJsonElement(row.payload).jsonObject
        fun s(k: String) = p[k]?.takeIf { it !is JsonNull }?.jsonPrimitive?.content
        when (row.kind) {
            Kind.capture_text -> settle(row, transport.captureText(s("text") ?: "", s("title"), s("note"), tz()))
            Kind.capture_files -> {
                val files = outbox.filesOf(row.id).map { OutFile(it.path, it.name, it.mime) }
                settle(row, transport.captureFiles(files, s("title"), s("note"), tz()))
            }
            Kind.done -> {
                transport.momentDone(s("moment")!!)
                outbox.sent(row.id, 204, "")
            }
            Kind.snooze -> {
                transport.momentSnooze(s("moment")!!, p["until"]!!.jsonPrimitive.longOrNull ?: 0L)
                outbox.sent(row.id, 204, "")
            }
        }
    }

    private suspend fun settle(row: OutboxRow, a: Answer) {
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
