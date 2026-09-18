package io.github.overcuriousity.engram.core.contained

import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.SocketEffect
import okhttp3.OkHttpClient
import okio.Buffer
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import java.io.File
import java.io.IOException
import java.security.MessageDigest
import kotlin.io.path.createTempDirectory

class DownloaderTest {
    private val server = MockWebServer()
    private val dir = createTempDirectory("models").toFile()
    private val body = ByteArray(200_000) { (it * 31 % 251).toByte() }
    private val client = OkHttpClient.Builder().retryOnConnectionFailure(false).build()
    private val loader = Downloader(dir, client)
    private lateinit var model: Model

    private fun sha(b: ByteArray) = MessageDigest.getInstance("SHA-256").digest(b).joinToString("") { "%02x".format(it) }
    private fun bytes(b: ByteArray, from: Int = 0) = Buffer().write(b, from, b.size - from)
    private val part get() = File(dir, "m.gguf.part")

    @Before fun up() {
        server.start()
        model = Model(Role.embed, "a model", "m.gguf", server.url("/m").toString(), sha(body), body.size.toLong(), "MIT")
    }
    @After fun down() = server.close()

    @Test fun aWholeFileArrivesVerifiedUnderItsOwnName() = runTest {
        server.enqueue(MockResponse.Builder().code(200).body(bytes(body)).build())
        val f = loader.fetch(model)
        assertArrayEquals(body, f.readBytes())
        assertEquals("m.gguf", f.name)
        assertFalse(part.exists())
        assertEquals(f, loader.installed(model))
        assertNull(server.takeRequest().headers["Range"])
    }

    @Test fun aCutDownloadResumesFromWhatItHas() = runTest {
        server.enqueue(MockResponse.Builder().code(200).body(bytes(body)).onResponseBody(SocketEffect.CloseStream()).build())
        assertThrows(IOException::class.java) { kotlinx.coroutines.runBlocking { loader.fetch(model) } }
        assertNull(loader.installed(model))
        server.takeRequest()
        // Whatever arrived before the cut; the test does not depend on how much.
        val have = part.length().toInt()
        server.enqueue(
            MockResponse.Builder().code(206).body(bytes(body, have))
                .addHeader("Content-Range", "bytes $have-${body.size - 1}/${body.size}").build(),
        )
        val f = loader.fetch(model)
        assertArrayEquals(body, f.readBytes())
        val range = server.takeRequest().headers["Range"]
        assertEquals(if (have > 0) "bytes=$have-" else null, range)
    }

    @Test fun aResumeAsksForExactlyWhatIsMissing() = runTest {
        dir.mkdirs(); part.writeBytes(body.copyOf(70_000))
        server.enqueue(MockResponse.Builder().code(206).body(bytes(body, 70_000)).build())
        assertArrayEquals(body, loader.fetch(model).readBytes())
        assertEquals("bytes=70000-", server.takeRequest().headers["Range"])
    }

    @Test fun aServerThatIgnoresRangeIsReadFromTheStart() = runTest {
        dir.mkdirs(); part.writeBytes(body.copyOf(70_000))
        server.enqueue(MockResponse.Builder().code(200).body(bytes(body)).build())
        val f = loader.fetch(model)
        assertEquals(body.size.toLong(), f.length())
        assertArrayEquals(body, f.readBytes())
    }

    @Test fun aFileThatDoesNotHashIsNotKept() = runTest {
        server.enqueue(MockResponse.Builder().code(200).body(bytes(ByteArray(body.size) { 7 })).build())
        val e = assertThrows(DownloadFailed::class.java) { kotlinx.coroutines.runBlocking { loader.fetch(model) } }
        assertEquals("a model did not verify", e.message)
        assertFalse(File(dir, "m.gguf").exists())
        assertFalse(part.exists())
    }

    @Test fun aRangeTheServerCannotSatisfyStartsOver() = runTest {
        dir.mkdirs(); part.writeBytes(ByteArray(body.size) { 7 })
        server.enqueue(MockResponse(code = 416))
        assertThrows(DownloadFailed::class.java) { kotlinx.coroutines.runBlocking { loader.fetch(model) } }
        assertFalse(part.exists())
    }

    @Test fun anythingElseTheServerSaysIsAFailureThatKeepsWhatItHas() = runTest {
        dir.mkdirs(); part.writeBytes(body.copyOf(10))
        server.enqueue(MockResponse(code = 503))
        assertThrows(DownloadFailed::class.java) { kotlinx.coroutines.runBlocking { loader.fetch(model) } }
        assertEquals(10L, part.length())
    }

    @Test fun whatIsInstalledIsNotFetchedAgain() = runTest {
        server.enqueue(MockResponse.Builder().code(200).body(bytes(body)).build())
        loader.fetch(model); loader.fetch(model)
        assertEquals(1, server.requestCount)
    }

    @Test fun progressIsBytesOnDiskIncludingWhatWasAlreadyThere() = runTest {
        dir.mkdirs(); part.writeBytes(body.copyOf(70_000))
        server.enqueue(MockResponse.Builder().code(206).body(bytes(body, 70_000)).build())
        val seen = mutableListOf<Long>()
        loader.fetch(model) { seen += it }
        assertEquals(70_000L, seen.first())
        assertEquals(body.size.toLong(), seen.last())
        assertEquals(seen.sorted(), seen)
    }

    @Test fun removedIsGoneHalfFetchedOrWhole() = runTest {
        server.enqueue(MockResponse.Builder().code(200).body(bytes(body)).build())
        loader.fetch(model)
        loader.remove(model)
        assertNull(loader.installed(model))
    }
}
