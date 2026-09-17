package io.github.overcuriousity.engram.core

import kotlinx.coroutines.Dispatchers
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

    private suspend fun send(req: Request): Answer = withContext(Dispatchers.IO) {
        val authed = req.newBuilder().header("Authorization", "Bearer ${connection.token}").build()
        try {
            client.newCall(authed).execute().use { res ->
                if (res.code == 401) throw Refused()
                Answer(res.code, res.body.string())
            }
        } catch (e: SSLPeerUnverifiedException) {
            // OkHttp's message names the pins it saw; the served one is on its
            // second line. Good enough for a screen that only has to be loud.
            throw PinMismatch(connection.pin ?: "", e.message?.lines()?.getOrNull(1)?.trim() ?: "?")
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

    private fun jsonBody(s: String): RequestBody = s.toRequestBody("application/json".toMediaType())
    private fun q(s: String) = Json.encodeToString(String.serializer(), s)
}
