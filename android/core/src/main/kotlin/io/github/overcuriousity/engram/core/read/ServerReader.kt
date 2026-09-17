package io.github.overcuriousity.engram.core.read

import io.github.overcuriousity.engram.core.PinMismatch
import io.github.overcuriousity.engram.core.Refused
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.CacheDao
import io.github.overcuriousity.engram.core.db.CacheRow
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import java.io.IOException

/**
 * The paired server, read through a cache that revalidates.
 *
 * Every JSON read on the server answers an `ETag`, so what is kept is the body
 * as it came and its tag: the next read sends the tag back, and a `304` costs a
 * round trip and no body. Nothing is fetched that was not asked for — there is
 * no background saving — so what can be shown without the server is exactly
 * what was opened before, and the screen says when.
 */
internal class ServerReader(
    private val transport: () -> Transport?,
    private val dao: CacheDao,
    private val clock: () -> Long = System::currentTimeMillis,
    private val onRefused: () -> Unit,
    private val onPinMismatch: (PinMismatch) -> Unit,
) : Reader {

    override fun <T> read(request: Request, decode: (String) -> T): Flow<Read<T>> = flow {
        val t = transport()
        if (t == null) {
            emit(Read(null, null, Reach.Refused, loading = false))
            return@flow
        }
        // Keyed by origin as well: after a re-pair elsewhere, the previous
        // server's notes must not appear under the new one's name.
        val origin = t.connection.origin
        val held = dao.get(request.key, origin)
        val heldValue = held?.let { runCatching { decode(it.body) }.getOrNull() }
        emit(Read(heldValue, held?.fetchedAt, Reach.Fresh, loading = true))

        val got = try {
            t.get(request.path, request.query, held?.etag)
        } catch (e: Refused) {
            onRefused()
            emit(Read(heldValue, held?.fetchedAt, Reach.Refused, loading = false))
            return@flow
        } catch (e: PinMismatch) {
            onPinMismatch(e)
            emit(Read(heldValue, held?.fetchedAt, Reach.Unreachable, loading = false))
            return@flow
        } catch (e: IOException) {
            emit(Read(heldValue, held?.fetchedAt, Reach.Unreachable, loading = false))
            return@flow
        }

        val now = clock()
        when (got.status) {
            304 -> {
                dao.touch(request.key, origin, now)
                emit(Read(heldValue, now, Reach.Fresh, loading = false))
            }
            200 -> {
                val value = runCatching { decode(got.body) }.getOrNull()
                if (value == null) {
                    // Not kept: a body this app cannot read is not worth showing again.
                    emit(Read(heldValue, held?.fetchedAt, Reach.Fresh, loading = false, error = "unreadable answer"))
                } else {
                    dao.put(CacheRow(request.key, origin, got.etag, got.body, now))
                    emit(Read(value, now, Reach.Fresh, loading = false))
                }
            }
            404 -> {
                // Gone on the server is gone here: keeping it would show a
                // deleted note for as long as the phone stayed offline.
                dao.delete(request.key, origin)
                emit(Read(null, null, Reach.Fresh, loading = false, error = said(got.body, 404)))
            }
            else -> emit(Read(heldValue, held?.fetchedAt, Reach.Fresh, loading = false, error = said(got.body, got.status)))
        }
    }

    override suspend fun <T> ask(path: String, json: String, decode: (String) -> T): Read<T> {
        val t = transport() ?: return Read(null, null, Reach.Refused, loading = false)
        val a = try {
            t.post(path, json)
        } catch (e: Refused) {
            onRefused()
            return Read(null, null, Reach.Refused, loading = false)
        } catch (e: PinMismatch) {
            onPinMismatch(e)
            return Read(null, null, Reach.Unreachable, loading = false)
        } catch (e: IOException) {
            return Read(null, null, Reach.Unreachable, loading = false)
        }
        if (a.status !in 200..299) return Read(null, null, Reach.Fresh, loading = false, error = said(a.body, a.status))
        val value = runCatching { decode(a.body) }.getOrNull()
        return Read(value, clock(), Reach.Fresh, loading = false, error = if (value == null) "unreadable answer" else null)
    }

    override suspend fun tell(path: String, json: String) {
        runCatching { transport()?.post(path, json) }
    }

    /** Drop everything held. Unpairing calls this; so does nothing else. */
    suspend fun forget() = dao.clear()

    /** Drop what has not been current for `days`. */
    suspend fun prune(days: Int = 30) = dao.deleteBefore(clock() - days * 86_400_000L)

    companion object {
        /** `{ "error": "…" }` is the server's one error shape; the status is the fallback. */
        internal fun said(body: String, status: Int): String =
            runCatching { Json.parseToJsonElement(body).jsonObject["error"]?.jsonPrimitive?.contentOrNull }
                .getOrNull() ?: "the server answered $status"
    }
}
