package io.github.overcuriousity.engram.core.contained

import android.util.Log
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.put
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MultipartBody
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.asRequestBody
import okhttp3.RequestBody.Companion.toRequestBody
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * The device pass's measuring instrument. Not a test of anything: it starts
 * the core on the phone over whatever model files were pushed beside it, and
 * writes down how long things take. The numbers go in the commit that sets
 * the defaults; see docs/superpowers/plans/2026-09-19-contained-7-device-pass.md.
 *
 * Files are looked for in this test package's external files directory, which
 * `adb push` can write and the test can read with no permission:
 *   embed.gguf            required
 *   rerank-<name>.gguf    any number; each is measured in a core of its own
 *   ask-<name>.gguf       any number; each answers the same questions
 *   speech.bin, speech.wav
 *   captures.txt          one capture per paragraph, blank line between
 *   questions.txt         one per line
 * The answer is bench.json in the same directory.
 */
@RunWith(AndroidJUnit4::class)
class DeviceBench {
    private val ctx = ApplicationProvider.getApplicationContext<android.content.Context>()
    private val dir = ctx.getExternalFilesDir(null)!!
    private val http = OkHttpClient.Builder().readTimeout(10, TimeUnit.MINUTES).build()
    private val json = "application/json".toMediaType()

    private class Live(val origin: String, val token: String)

    private fun <T> timed(block: () -> T): Pair<T, Long> {
        val t0 = System.nanoTime()
        val v = block()
        return v to (System.nanoTime() - t0) / 1_000_000
    }

    private fun start(data: File, setup: Setup): Live =
        Core.start(data.path, setup).let { Live("http://127.0.0.1:${it.port}", it.token) }

    private fun Live.get(path: String): String =
        http.newCall(Request.Builder().url(origin + path).header("Authorization", "Bearer $token").build()).execute().use { it.body.string() }

    private fun Live.post(path: String, body: String): String =
        http.newCall(Request.Builder().url(origin + path).header("Authorization", "Bearer $token").post(body.toRequestBody(json)).build())
            .execute().use { it.body.string() }

    private fun Live.settled(): Boolean =
        Json.parseToJsonElement(get("/api/v1/corpora?limit=200")).jsonObject["items"]!!.jsonArray
            .none { it.jsonObject["status"]!!.jsonPrimitive.content in setOf("raw", "embedding", "extracting", "describing") }

    /** Ids of the top hits, in order: what two rerankers are compared by. */
    private fun Live.search(q: String): List<String> =
        Json.parseToJsonElement(get("/api/v1/search?limit=10&q=" + java.net.URLEncoder.encode(q, "UTF-8"))).jsonObject["items"]!!.jsonArray
            .map { it.jsonObject["id"]!!.jsonPrimitive.content }

    @Test fun measure() {
        assumeTrue("not built for this device", Core.available)
        val embed = File(dir, "embed.gguf")
        assumeTrue("push embed.gguf to ${dir.path}", embed.isFile)
        Core.init(ctx)
        val captures = File(dir, "captures.txt").takeIf { it.isFile }?.readText()?.split(Regex("\n\\s*\n"))?.map { it.trim() }?.filter { it.isNotEmpty() }.orEmpty()
        val questions = File(dir, "questions.txt").takeIf { it.isFile }?.readLines()?.map { it.trim() }?.filter { it.isNotEmpty() }.orEmpty()
        val data = File(ctx.filesDir, "bench-core").apply { deleteRecursively() }
        val out = mutableMapOf<String, kotlinx.serialization.json.JsonElement>()

        // Start, capture to searchable, search without a reranker.
        val (live, startMs) = timed { start(data, Setup(embed = embed.path)) }
        out["core_start_ms"] = JsonPrimitive(startMs)
        val (_, ingestMs) = timed {
            captures.forEach { live.post("/api/v1/corpora", buildJsonObject { put("text", it); put("source", "web") }.toString()) }
            while (!live.settled()) Thread.sleep(250)
        }
        out["captures"] = JsonPrimitive(captures.size)
        out["capture_to_searchable_ms_total"] = JsonPrimitive(ingestMs)
        val plain = questions.associateWith { q -> timed { live.search(q) } }
        out["search_ms_no_reranker"] = buildJsonArray { plain.values.forEach { add(JsonPrimitive(it.second)) } }
        Core.shutdown()

        // Each reranker in a core of its own, over the same base.
        out["rerankers"] = JsonObject(
            dir.listFiles { f -> f.name.startsWith("rerank-") && f.name.endsWith(".gguf") }.orEmpty().sortedBy { it.name }.associate { f ->
                val l = start(data, Setup(embed = embed.path, rerank = f.path))
                val runs = questions.associateWith { q -> timed { l.search(q) } }
                Core.shutdown()
                f.name to buildJsonObject {
                    put("search_ms", buildJsonArray { runs.values.forEach { add(JsonPrimitive(it.second)) } })
                    // How often it changed the order, and the orders themselves
                    // for comparison with bge-reranker-v2-m3's on the desktop.
                    put("reordered", runs.count { (q, r) -> r.first != plain[q]!!.first })
                    put("orders", buildJsonObject { runs.forEach { (q, r) -> put(q, buildJsonArray { r.first.forEach { add(JsonPrimitive(it)) } }) } })
                }
            },
        )

        // Each ask model answers every question; tokens a second from the stream's own frames.
        out["ask"] = JsonObject(
            dir.listFiles { f -> f.name.startsWith("ask-") && f.name.endsWith(".gguf") }.orEmpty().sortedBy { it.name }.associate { f ->
                val l = start(data, Setup(embed = embed.path, ask = f.path))
                val runs = questions.map { q ->
                    var tokens = 0; var firstAt = 0L; var reasoning = 0
                    val (_, ms) = timed {
                        val req = Request.Builder().url(l.origin + "/api/v1/ask/stream?door=app").header("Authorization", "Bearer ${l.token}")
                            .header("Accept", "text/event-stream").post(buildJsonObject { put("q", q) }.toString().toRequestBody(json)).build()
                        val t0 = System.nanoTime()
                        http.newCall(req).execute().use { res ->
                            val src = res.body.source()
                            var event = ""
                            while (true) {
                                val line = src.readUtf8Line() ?: break
                                if (line.startsWith("event:")) event = line.substring(6).trim()
                                if (line.startsWith("data:") && event == "token") { if (tokens++ == 0) firstAt = (System.nanoTime() - t0) / 1_000_000 }
                                if (line.startsWith("data:") && event == "reasoning") reasoning++
                            }
                        }
                    }
                    buildJsonObject {
                        put("total_ms", ms); put("first_token_ms", firstAt); put("token_frames", tokens); put("reasoning_frames", reasoning)
                        put("frames_per_s", if (ms > firstAt && tokens > 1) (tokens - 1) * 1000.0 / (ms - firstAt) else 0.0)
                    }
                }
                Core.shutdown()
                f.name to buildJsonArray { runs.forEach { add(it) } }
            },
        )

        // Dictation: model load and all, as a held button pays for it.
        val speech = File(dir, "speech.bin"); val wav = File(dir, "speech.wav")
        if (speech.isFile && wav.isFile) {
            val l = start(data, Setup(embed = embed.path, speech = speech.path))
            val body = MultipartBody.Builder().setType(MultipartBody.FORM)
                .addFormDataPart("audio", "recording", wav.asRequestBody("audio/wav".toMediaType())).build()
            val runs = (1..3).map {
                timed {
                    http.newCall(Request.Builder().url(l.origin + "/api/v1/transcribe").header("Authorization", "Bearer ${l.token}").post(body).build())
                        .execute().use { it.body.string() }
                }
            }
            Core.shutdown()
            out["speech"] = buildJsonObject {
                put("wav_bytes", wav.length()); put("heard", runs.first().first)
                put("ms", buildJsonArray { runs.forEach { add(JsonPrimitive(it.second)) } })
            }
        }

        val report = JsonObject(out).toString()
        File(dir, "bench.json").writeText(report)
        Log.i("engram-bench", report)
    }
}
