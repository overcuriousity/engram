package io.github.overcuriousity.engram.core.contained

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import okhttp3.Request
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.security.MessageDigest

class DownloadFailed(message: String) : IOException(message)

/**
 * Fetches a model into [dir]. What has arrived is kept in `<file>.part` and
 * asked to be continued with a `Range`; the name without `.part` exists only
 * after the whole file's SHA-256 is the manifest's, so nothing that reads the
 * directory can pick up a model that was not checked.
 */
class Downloader(private val dir: File, private val client: OkHttpClient) {
    fun installed(model: Model): File? = File(dir, model.file).takeIf { it.isFile && it.length() == model.bytes }
    fun remove(model: Model) { File(dir, model.file).delete(); part(model).delete() }
    private fun part(model: Model) = File(dir, model.file + ".part")

    suspend fun fetch(model: Model, onProgress: (Long) -> Unit = {}): File = withContext(Dispatchers.IO) {
        installed(model)?.let { return@withContext it }
        dir.mkdirs()
        val part = part(model)
        val have = part.length()
        val req = Request.Builder().url(model.url).apply { if (have > 0) header("Range", "bytes=$have-") }.build()
        client.newCall(req).execute().use { res ->
            when (res.code) {
                206 -> {}
                200 -> part.delete() // the server sent everything; what was held is not a prefix of this stream
                416 -> { part.delete(); throw DownloadFailed("the server could not continue ${model.name}") }
                else -> throw DownloadFailed("${model.name}: the server answered ${res.code}")
            }
            var written = part.length()
            onProgress(written)
            FileOutputStream(part, true).use { out ->
                val src = res.body.source()
                val buf = ByteArray(1 shl 16)
                while (true) {
                    ensureActive()
                    val n = src.read(buf)
                    if (n < 0) break
                    out.write(buf, 0, n)
                    written += n
                    onProgress(written)
                }
            }
        }
        if (sha256(part) != model.sha256) {
            part.delete()
            throw DownloadFailed("${model.name} did not verify")
        }
        val done = File(dir, model.file)
        if (!part.renameTo(done)) throw DownloadFailed("${model.name} could not be put in place")
        done
    }

    private fun sha256(f: File): String {
        val md = MessageDigest.getInstance("SHA-256")
        f.inputStream().use { s -> val b = ByteArray(1 shl 16); while (true) { val n = s.read(b); if (n < 0) break; md.update(b, 0, n) } }
        return md.digest().joinToString("") { "%02x".format(it) }
    }
}
