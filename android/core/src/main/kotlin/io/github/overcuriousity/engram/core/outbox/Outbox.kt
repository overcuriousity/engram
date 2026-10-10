package io.github.overcuriousity.engram.core.outbox

import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.OutboxFile
import io.github.overcuriousity.engram.core.db.OutboxRow
import io.github.overcuriousity.engram.core.db.State
import kotlinx.coroutines.flow.Flow
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.put
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.util.UUID

class Incoming(val name: String, val mime: String, val open: () -> InputStream)

/** The three answers an artifact admits that have an undo. Each is a route of its own. */
enum class ArtifactOp { deprecate, reactivate, unsupersede }

/**
 * The load-bearing idea. A share is copied out of the sender's URI into our
 * own storage and written as a row before anything touches the network; the
 * caller is answered at once, and a worker owes the server the rest.
 */
class Outbox(private val db: Db, private val dir: File, private val clock: () -> Long = System::currentTimeMillis) {
    private val dao = db.outboxDao()
    val rows: Flow<List<OutboxRow>> = dao.all()

    /** [fromAsk] is the question this text answers, where the box was filled from an answer: the web's *edit first*. */
    suspend fun enqueueText(text: String, title: String?, note: String?, fromAsk: String? = null): String =
        insert(Kind.capture_text, buildJsonObject { put("text", text); put("title", title); put("note", note); if (fromAsk != null) put("from_ask", fromAsk) })

    suspend fun enqueueFiles(files: List<Incoming>, title: String?, note: String?): String {
        val id = UUID.randomUUID().toString()
        val folder = File(dir, id)
        val copied = try {
            folder.mkdirs()
            files.mapIndexed { i, f ->
                val safe = f.name.replace(Regex("[^A-Za-z0-9._-]"), "_").ifEmpty { "file" }
                val target = File(folder, "$i-$safe")
                f.open().use { input -> target.outputStream().use { input.copyTo(it) } }
                OutboxFile(id, target.path, f.name, f.mime)
            }
        } catch (e: IOException) {
            folder.deleteRecursively()
            throw e
        }
        val now = clock()
        // One transaction: a drain pass must never see the row without its files.
        dao.insertWithFiles(
            OutboxRow(id, Kind.capture_files, buildJsonObject { put("title", title); put("note", note) }.toString(), now, nextAt = now),
            copied,
        )
        return id
    }

    // ── Decisions ────────────────────────────────────────────────────────────
    // What a person still decides about the base — hiding a note, bringing one
    // back, taking back something the base did — is a row here rather than a
    // call from the screen. A decision made on a train is a decision; the
    // queue is the one place that knows what is owed, and the one place an
    // answer can be taken back out of before it goes.

    suspend fun enqueueArtifactOp(artifactId: String, op: ArtifactOp) =
        insert(Kind.artifact_op, buildJsonObject { put("artifact", artifactId); put("op", op.name) })

    /**
     * Delete for good. Not an [ArtifactOp]: those three are answers with an
     * undo, and this is the one that has none — which is why the screen asks
     * before it is enqueued, and why the queue is the last place it can be
     * taken back from.
     */
    suspend fun enqueueArtifactDelete(artifactId: String) =
        insert(Kind.artifact_delete, buildJsonObject { put("artifact", artifactId) })

    /**
     * Any other owed write, by its route. [label] is what the queue screen
     * says about it; [body] is JSON or null for a bare POST/DELETE.
     */
    suspend fun enqueueCall(label: String, method: String, path: String, body: String? = null) =
        insert(Kind.call, buildJsonObject { put("label", label); put("method", method); put("path", path); put("body", body) })

    suspend fun enqueueMergeUndo(mergeId: String) =
        insert(Kind.merge_undo, buildJsonObject { put("merge", mergeId) })

    /**
     * Take an answer back. Only while it is still queued: what the server
     * already has is not ours to undo from here, and a row the drainer is
     * settling is the server's business. The screen's Undo is this and nothing
     * cleverer — which is why the window it offers is honest.
     */
    suspend fun undo(id: String): Boolean {
        val row = dao.get(id) ?: return false
        if (row.state != State.queued) return false
        delete(id)
        return true
    }

    suspend fun enqueueDone(momentId: String) = insert(Kind.done, buildJsonObject { put("moment", momentId) })
    suspend fun enqueueSnooze(momentId: String, until: Long) =
        insert(Kind.snooze, buildJsonObject { put("moment", momentId); put("until", until) })

    private suspend fun insert(kind: Kind, payload: JsonObject): String {
        val id = UUID.randomUUID().toString()
        val now = clock()
        dao.insert(OutboxRow(id, kind, payload.toString(), now, nextAt = now))
        return id
    }

    suspend fun patchNote(id: String, note: String): Boolean {
        val row = dao.get(id) ?: return false
        if (row.state != State.queued) return false
        val p = Json.parseToJsonElement(row.payload).jsonObject.toMutableMap()
        p["note"] = JsonPrimitive(note)
        dao.update(row.copy(payload = JsonObject(p).toString()))
        return true
    }

    suspend fun filesOf(id: String) = dao.filesOf(id)
    suspend fun dueQueued(now: Long) = dao.dueQueued(now)

    suspend fun sent(id: String, status: Int, body: String) {
        val row = dao.get(id) ?: return
        dao.update(row.copy(state = State.sent, status = status, answer = body, error = null))
        dropFiles(id)
    }

    suspend fun failed(id: String, error: String) {
        val row = dao.get(id) ?: return
        val attempts = row.attempts + 1
        dao.update(row.copy(attempts = attempts, nextAt = clock() + Backoff.delayMs(attempts), error = error))
    }

    /** A 4xx that is not 401: the server refused this row for what it is. Files stay, so the person can see what. */
    suspend fun held(id: String, status: Int, body: String) {
        val row = dao.get(id) ?: return
        dao.update(row.copy(state = State.held, status = status, answer = body, error = body))
    }

    /**
     * Send a held row again, as if it had just been owed. A hold is the server's
     * answer to one request, and that answer can change: a client or a server
     * that is fixed, a limit that is raised. Deleting and sharing again is not
     * a way back — the share sheet is gone by the time the Queue shows it.
     */
    suspend fun retry(id: String): Boolean {
        val row = dao.get(id) ?: return false
        if (row.state != State.held) return false
        dao.update(row.copy(state = State.queued, nextAt = clock(), attempts = 0, status = null, answer = null, error = null))
        return true
    }

    /** Every held row, owed again. See [retry]. */
    suspend fun retryHeld() = dao.requeueHeld(clock())

    suspend fun refuseAll() = dao.refuseAll()
    suspend fun requeueRefused() = dao.requeueRefused(clock())

    suspend fun delete(id: String) { dao.delete(id); dropFiles(id) }

    suspend fun sweepSent(olderThanMs: Long) {
        dao.sentBefore(clock() - olderThanMs).forEach { delete(it) }
    }

    private suspend fun dropFiles(id: String) {
        dao.deleteFiles(id)
        File(dir, id).deleteRecursively()
    }
}
