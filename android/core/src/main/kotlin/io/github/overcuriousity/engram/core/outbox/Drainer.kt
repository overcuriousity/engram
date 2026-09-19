package io.github.overcuriousity.engram.core.outbox

import io.github.overcuriousity.engram.core.Answer
import io.github.overcuriousity.engram.core.OutFile
import io.github.overcuriousity.engram.core.PinMismatch
import io.github.overcuriousity.engram.core.Refused
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.OutboxRow
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
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

    /** A row this build cannot make a request out of. Retrying it changes nothing. */
    private class Malformed(message: String) : Exception(message)

    suspend fun drainOnce(): Outcome {
        for (row in outbox.dueQueued(clock())) {
            try {
                deliver(row)
            } catch (e: Refused) {
                outbox.refuseAll()
                return Outcome.Refused
            } catch (e: PinMismatch) {
                return Outcome.Pinned(e)
            } catch (e: kotlin.coroutines.cancellation.CancellationException) {
                throw e
            } catch (e: IOException) {
                // A failure does not stop the pass: the row moves out on its
                // rung and the next one gets its turn.
                outbox.failed(row.id, e.message ?: e.javaClass.simpleName)
            } catch (e: Exception) {
                // Anything else is this row itself: a payload we cannot read,
                // a media type no parser accepts. The ladder has no rung that
                // makes it truer, and letting it out of the pass would leave
                // it queued and due — first in line on every later kick, to
                // throw again, with every capture behind it waiting on a row
                // that can never go. It is held instead, and the queue moves.
                outbox.held(row.id, 0, e.message ?: e.javaClass.simpleName)
            }
        }
        val pending = outbox.dueQueued(Long.MAX_VALUE).minOfOrNull { it.nextAt }
        return if (pending == null) Outcome.Done else Outcome.Later(pending)
    }

    private suspend fun deliver(row: OutboxRow) {
        val p = runCatching { Json.parseToJsonElement(row.payload).jsonObject }
            .getOrElse { throw Malformed("the row's payload is not an object") }
        fun s(k: String) = p[k]?.takeIf { it !is JsonNull }?.jsonPrimitive?.content
        fun need(k: String) = s(k) ?: throw Malformed("the row carries no $k")
        fun pair() = p["pair"]?.jsonPrimitive?.longOrNull ?: throw Malformed("the row carries no pair")
        when (row.kind) {
            Kind.capture_text -> settle(row, transport.captureText(s("text") ?: "", s("title"), s("note"), tz(), s("from_ask")))
            Kind.capture_files -> {
                val files = outbox.filesOf(row.id).map { OutFile(it.path, it.name, it.mime) }
                settle(row, transport.captureFiles(files, s("title"), s("note")))
            }
            // A moment the server no longer has is a moment nobody still owes:
            // 404 settles the row rather than holding it for a review that has
            // nothing to look at.
            Kind.done -> settle(row, transport.momentDone(need("moment")), goneIsSettled = true)
            Kind.snooze -> settle(
                row,
                transport.momentSnooze(need("moment"), p["until"]?.jsonPrimitive?.longOrNull ?: 0L),
                goneIsSettled = true,
            )
            // A judging answer whose subject the server no longer has is
            // settled for the reason a vanished moment is: two doors open onto
            // one base, and a pair answered on the web while this phone was
            // offline is not work still owed. Holding it would show a person a
            // failure for a decision that has already been made.
            Kind.pair_supersede -> settle(row, transport.pairSupersede(pair(), s("keep")), goneIsSettled = true)
            Kind.pair_synthesize -> settle(row, transport.pairSynthesize(pair()), goneIsSettled = true)
            Kind.pair_discard -> settle(row, transport.pairDiscard(pair()), goneIsSettled = true)
            Kind.pair_dismiss -> settle(row, transport.pairDismiss(pair()), goneIsSettled = true)
            Kind.gap_dismiss -> settle(row, transport.gapDismiss(need("kind"), need("id")), goneIsSettled = true)
            Kind.gap_forget -> settle(row, transport.gapForget(members(p)), goneIsSettled = true)
            Kind.artifact_op -> settle(
                row,
                transport.artifactOp(need("artifact"), need("op")),
                goneIsSettled = true,
            )
            // Deleted on the web while this phone was offline is deleted.
            Kind.artifact_delete -> settle(row, transport.artifactDelete(need("artifact")), goneIsSettled = true)
            // A subject the server no longer has is settled for the reason
            // every judging answer's is.
            Kind.call -> settle(row, transport.call(need("method"), need("path"), s("body")), goneIsSettled = true)
            Kind.merge_undo -> settle(row, transport.mergeUndo(need("merge")), goneIsSettled = true)
            Kind.corpus_resolve -> settle(
                row,
                transport.corpusResolve(need("corpus"), need("action")),
                goneIsSettled = true,
            )
        }
    }

    /**
     * What the server said, turned into the row's state: kept, held for the
     * person to look at, or thrown back onto the ladder. Every kind comes
     * through here, so no kind can quietly decide a 4xx is worth retrying —
     * a snooze whose `until` has gone past while the phone was offline is
     * refused the same way for as long as it is re-sent.
     */
    private suspend fun settle(row: OutboxRow, a: Answer, goneIsSettled: Boolean = false) {
        when {
            a.status in 200..299 -> outbox.sent(row.id, a.status, a.body)
            goneIsSettled && a.status == 404 -> outbox.sent(row.id, a.status, a.body)
            a.status in 400..499 -> outbox.held(row.id, a.status, message(a.body))
            else -> throw IOException("server answered ${a.status}")
        }
    }

    /** The members a forget was shown, as the route names them. */
    private fun members(p: kotlinx.serialization.json.JsonObject): List<Pair<String, String>> =
        (p["members"] as? JsonArray ?: throw Malformed("the row carries no members")).map {
            val m = it.jsonObject
            val kind = m["kind"]?.jsonPrimitive?.content
            val id = m["id"]?.jsonPrimitive?.content
            if (kind == null || id == null) throw Malformed("a member carries no kind and id")
            kind to id
        }

    /** The server's `{"error": "..."}` if that is what came back, else the body. */
    private fun message(body: String): String =
        runCatching { Json.parseToJsonElement(body).jsonObject["error"]?.jsonPrimitive?.content }.getOrNull() ?: body
}
