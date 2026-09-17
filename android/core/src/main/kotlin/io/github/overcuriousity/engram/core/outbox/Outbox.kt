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

/**
 * The load-bearing idea. A share is copied out of the sender's URI into our
 * own storage and written as a row before anything touches the network; the
 * caller is answered at once, and a worker owes the server the rest.
 */
class Outbox(private val db: Db, private val dir: File, private val clock: () -> Long = System::currentTimeMillis) {
    private val dao = db.outboxDao()
    val rows: Flow<List<OutboxRow>> = dao.all()

    suspend fun enqueueText(text: String, title: String?, note: String?): String =
        insert(Kind.capture_text, buildJsonObject { put("text", text); put("title", title); put("note", note) })

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
