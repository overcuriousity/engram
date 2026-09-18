package io.github.overcuriousity.engram.core

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.builtins.serializer
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.CertificatePinner
import okhttp3.HttpUrl.Companion.toHttpUrl
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.MultipartBody
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody
import okhttp3.RequestBody.Companion.asRequestBody
import okhttp3.RequestBody.Companion.toRequestBody
import java.io.File
import java.io.IOException
import java.util.concurrent.TimeUnit
import javax.net.ssl.SSLPeerUnverifiedException

/** The server answered 401: paired, and refused. Not retried. */
class Refused : IOException("the server refused the credential")

/** The certificate served is not the one pinned. Not retried, not recoverable. */
class PinMismatch(val expected: String, val served: String) :
    IOException("pinned $expected, served $served")

data class Answer(val status: Int, val body: String)

/** A read's answer. `304` carries no body and the tag that was sent. */
internal data class Got(val status: Int, val body: String, val etag: String?)

/** What a file whose declared type does not parse is sent as. */
private val OCTET_STREAM = "application/octet-stream".toMediaType()
data class OutFile(val path: String, val name: String, val mime: String)

fun userAgent(versionName: String, model: String) = "engram-android/$versionName ($model)"

internal fun baseClient(userAgent: String, pin: String?, host: String?): OkHttpClient {
    val b = OkHttpClient.Builder()
        .connectTimeout(15, TimeUnit.SECONDS)
        .readTimeout(60, TimeUnit.SECONDS)
        .retryOnConnectionFailure(false)
        .addInterceptor(Interceptor { chain ->
            chain.proceed(chain.request().newBuilder().header("User-Agent", userAgent).build())
        })
    if (pin != null && host != null) {
        b.certificatePinner(CertificatePinner.Builder().add(host, "sha256/$pin").build())
    }
    return b.build()
}

/**
 * The one client. Everything after pairing goes through here, carrying the
 * bearer and the pin; retry is the outbox's and lives nowhere in this file.
 */
internal class Transport(
    val connection: Connection,
    val userAgent: String,
    client: OkHttpClient? = null,
) {
    private val base = connection.origin.toHttpUrl()
    private val client: OkHttpClient = client ?: baseClient(userAgent, connection.pin, base.host)
    private val json = Json { ignoreUnknownKeys = true }

    private fun url(path: String, query: Map<String, String?> = emptyMap()) =
        base.newBuilder().encodedPath(path).apply {
            query.forEach { (k, v) -> if (v != null) addQueryParameter(k, v) }
        }.build()

    private fun authed(req: Request): Request =
        req.newBuilder().header("Authorization", "Bearer ${connection.token}").build()

    // OkHttp's message names the pins it saw; the served one is on its second
    // line. Good enough for a screen that only has to be loud.
    private fun mismatch(e: SSLPeerUnverifiedException) =
        PinMismatch(connection.pin ?: "", e.message?.lines()?.getOrNull(1)?.trim() ?: "?")

    private suspend fun send(req: Request): Answer = withContext(Dispatchers.IO) {
        try {
            client.newCall(authed(req)).execute().use { res ->
                if (res.code == 401) throw Refused()
                Answer(res.code, res.body.string())
            }
        } catch (e: SSLPeerUnverifiedException) {
            throw mismatch(e)
        }
    }

    /**
     * A read, revalidating when `etag` is what was held. Every JSON read on the
     * server answers a tag and honours `If-None-Match`; a `304` costs a round
     * trip and no body, which is the whole reason the cache keeps tags.
     */
    suspend fun get(path: String, query: Map<String, String?> = emptyMap(), etag: String? = null): Got =
        withContext(Dispatchers.IO) {
            val req = Request.Builder().url(url(path, query)).get()
                .apply { if (etag != null) header("If-None-Match", etag) }
                .build()
            try {
                client.newCall(authed(req)).execute().use { res ->
                    if (res.code == 401) throw Refused()
                    if (res.code == 304) Got(304, "", etag) else Got(res.code, res.body.string(), res.header("ETag"))
                }
            } catch (e: SSLPeerUnverifiedException) {
                throw mismatch(e)
            }
        }

    /** A POST whose answer is data rather than an outcome — the offer. Not a write the device owes. */
    suspend fun post(path: String, json: String): Answer =
        send(Request.Builder().url(url(path)).post(jsonBody(json)).build())

    // An answer may think for longer than any read timeout worth having on an
    // ordinary call; the stream's own silence is bounded by the server's
    // keep-alive comments, and by the person leaving the screen.
    private val streaming: OkHttpClient by lazy {
        this.client.newBuilder().readTimeout(0, TimeUnit.SECONDS).build()
    }

    /**
     * A POST that answers as server-sent events, read by hand: `event:` names
     * the frame, `data:` lines join with a newline, a blank line dispatches,
     * and a line opening with `:` is a comment the server keeps the socket
     * warm with. An answer that is not a stream dispatches nothing and comes
     * back whole — status and body — because the body is the server saying why.
     * A stream comes back as its status and an empty body.
     *
     * Cancelling the caller cancels the call. A blocked socket read does not
     * notice a coroutine being cancelled, and an ask nobody is reading goes on
     * holding the server's lane until its timeout — so a watcher closes the
     * socket, and the read fails out from under the loop.
     */
    suspend fun stream(
        path: String,
        query: Map<String, String?>,
        json: String,
        onFrame: suspend (event: String, data: String) -> Unit,
    ): Answer = coroutineScope {
        val req = Request.Builder().url(url(path, query)).post(jsonBody(json))
            .header("Accept", "text/event-stream").build()
        val call = streaming.newCall(authed(req))
        val watcher = launch { try { awaitCancellation() } finally { call.cancel() } }
        try {
            withContext(Dispatchers.IO) {
                try {
                    call.execute().use { res ->
                        if (res.code == 401) throw Refused()
                        if (!res.isSuccessful) return@use Answer(res.code, res.body.string())
                        val source = res.body.source()
                        var event = "message"
                        val data = StringBuilder()
                        var has = false
                        while (true) {
                            val line = source.readUtf8Line() ?: break
                            when {
                                line.isEmpty() -> {
                                    if (has) onFrame(event, data.toString())
                                    event = "message"; data.setLength(0); has = false
                                }
                                line.startsWith(":") -> {}
                                line.startsWith("event:") -> event = line.substring(6).trim()
                                line.startsWith("data:") -> {
                                    if (has) data.append('\n')
                                    data.append(line.substring(5).removePrefix(" "))
                                    has = true
                                }
                            }
                        }
                        Answer(res.code, "")
                    }
                } catch (e: SSLPeerUnverifiedException) {
                    throw mismatch(e)
                }
            }
        } finally {
            watcher.cancel()
        }
    }

    suspend fun captureText(text: String, title: String?, note: String?, tz: String): Answer =
        send(
            Request.Builder()
                .url(url("/api/v1/capture", mapOf("tz" to tz, "title" to title, "note" to note)))
                .post(text.toRequestBody("text/plain; charset=utf-8".toMediaType()))
                .build(),
        )

    suspend fun captureFiles(files: List<OutFile>, title: String?, note: String?, tz: String): Answer {
        val body = MultipartBody.Builder().setType(MultipartBody.FORM).apply {
            addFormDataPart("tz", tz)
            if (title != null) addFormDataPart("title", title)
            if (note != null) addFormDataPart("note", note)
            // A provider is free to hand back a type no parser accepts, and one
            // of those must not cost the capture: unreadable is untyped.
            files.forEach { f ->
                val mime = f.mime.toMediaTypeOrNull() ?: OCTET_STREAM
                addFormDataPart("file", f.name, File(f.path).asRequestBody(mime))
            }
        }.build()
        return send(Request.Builder().url(url("/api/v1/capture")).post(body).build())
    }

    /**
     * The microphone's door: one recording in, the words in it back. Not a
     * write the device owes — a dictation nobody is waiting for is a dictation
     * nobody wants — so this is sent once, now, and never queued. `audio` is
     * the part the server reads, with a filename because a part without one is
     * a field rather than a file to its multipart reader.
     */
    suspend fun transcribe(audio: ByteArray, mime: String): Answer {
        val body = MultipartBody.Builder().setType(MultipartBody.FORM)
            .addFormDataPart("audio", "recording", audio.toRequestBody(mime.toMediaTypeOrNull() ?: OCTET_STREAM))
            .build()
        return send(Request.Builder().url(url("/api/v1/transcribe")).post(body).build())
    }

    suspend fun vapid(): String {
        val a = send(Request.Builder().url(url("/api/v1/push/vapid")).get().build())
        if (a.status != 200) throw IOException("vapid: ${a.status}")
        return json.parseToJsonElement(a.body).jsonObject["public_key"]!!.jsonPrimitive.content
    }

    suspend fun registerPush(endpoint: String, p256dh: String, auth: String) {
        val body = """{"endpoint":${q(endpoint)},"p256dh":${q(p256dh)},"auth":${q(auth)}}"""
        val a = send(Request.Builder().url(url("/api/v1/push/unifiedpush")).put(jsonBody(body)).build())
        if (a.status !in 200..299) throw IOException("register push: ${a.status} ${a.body}")
    }

    suspend fun unregisterPush() {
        send(Request.Builder().url(url("/api/v1/push/unifiedpush")).delete().build())
    }

    /** The answer as it came. What a status means to the outbox is the outbox's to say. */
    suspend fun momentDone(id: String): Answer =
        send(Request.Builder().url(url("/api/v1/moments/$id/done")).post(jsonBody("")).build())

    suspend fun momentSnooze(id: String, until: Long): Answer =
        send(
            Request.Builder().url(url("/api/v1/moments/$id/snooze"))
                .post(jsonBody("""{"until":$until}""")).build(),
        )

    // ── Judging ──────────────────────────────────────────────────────────────
    // Every one of these is a decision somebody made, delivered by the outbox.
    // The answer comes back as it came; what a status means is the outbox's.

    /** `keep` absent, not null: the server reads an absent side as the one the judge proposed. */
    suspend fun pairSupersede(id: Long, keep: String?): Answer =
        judge("/api/v1/pairs/$id/supersede", keep?.let { """{"keep":${q(it)}}""" } ?: "{}")

    suspend fun pairSynthesize(id: Long): Answer = judge("/api/v1/pairs/$id/synthesize", "{}")
    suspend fun pairDiscard(id: Long): Answer = judge("/api/v1/pairs/$id/discard", "{}")
    suspend fun pairDismiss(id: Long): Answer = judge("/api/v1/pairs/$id/dismiss", "{}")
    suspend fun gapDismiss(kind: String, id: String): Answer = judge("/api/v1/gaps/$kind/$id/dismiss", "{}")

    /** A whole cluster, named by the members the person was shown. */
    suspend fun gapForget(members: List<kotlin.Pair<String, String>>): Answer =
        judge(
            "/api/v1/gaps/forget",
            members.joinToString(",", """{"members":[""", "]}") { (k, i) -> """{"kind":${q(k)},"id":${q(i)}}""" },
        )

    /** One of `verify`, `deprecate`, `reactivate`, `unsupersede`, each a route of its own. */
    suspend fun artifactOp(id: String, op: String): Answer = judge("/api/v1/artifacts/$id/$op", "{}")

    /** Gone from both stores. The answer comes back as it came, like every other decision. */
    suspend fun artifactDelete(id: String): Answer =
        send(Request.Builder().url(url("/api/v1/artifacts/$id")).delete().build())

    suspend fun mergeUndo(id: String): Answer = judge("/api/v1/merges/$id/undo", "{}")
    suspend fun corpusResolve(id: String, action: String): Answer =
        judge("/api/v1/corpora/$id/resolve", """{"action":${q(action)}}""")

    private suspend fun judge(path: String, json: String): Answer =
        send(Request.Builder().url(url(path)).post(jsonBody(json)).build())

    private fun jsonBody(s: String): RequestBody = s.toRequestBody("application/json".toMediaType())
    private fun q(s: String) = Json.encodeToString(String.serializer(), s)
}
