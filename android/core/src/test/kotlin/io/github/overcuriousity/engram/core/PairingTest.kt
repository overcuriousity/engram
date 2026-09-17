package io.github.overcuriousity.engram.core

import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test

class PairingTest {
    private val server = MockWebServer()
    @Before fun up() = server.start()
    @After fun down() = server.close()
    private fun uri(f: String? = null) =
        PairUri(server.url("/").toString().trimEnd('/'), "code123", "0.1.0", f)

    @Test fun aClaimPostsCodeAndDeviceAndKeepsTheAnswer() = runTest {
        server.enqueue(MockResponse(code = 201, body = """{"token":"engram_new","version":"0.1.0"}"""))
        val c = Pairing.claim(uri(), "engram for Android 0.1.0 · Pixel 8", "engram-android/0.1.0 (Pixel 8)")
        assertEquals("engram_new", c.token)
        assertEquals("0.1.0", c.serverVersion)
        assertEquals(uri().origin, c.origin)
        assertNull(c.pin)   // loopback over plain http: nothing to pin
        val r = server.takeRequest()
        assertEquals("/api/v1/pair/claim", r.target)
        assertNull(r.headers["Authorization"])
        assertEquals("engram-android/0.1.0 (Pixel 8)", r.headers["User-Agent"])
        assertEquals("""{"code":"code123","device":"engram for Android 0.1.0 · Pixel 8"}""", r.body!!.utf8())
    }

    @Test fun a401IsClaimRefused() = runTest {
        server.enqueue(MockResponse(code = 401))
        try { Pairing.claim(uri(), "d", "ua"); fail() } catch (e: ClaimRefused) {}
    }

    @Test fun aFingerprintInTheUriBecomesThePin() = runTest {
        // Over plain http the pin is never checked, but it is kept: the app
        // will refuse a later https handshake that does not match it.
        server.enqueue(MockResponse(code = 201, body = """{"token":"t","version":"1"}"""))
        val f = "F".repeat(43)
        assertEquals(f, Pairing.claim(uri(f), "d", "ua").pin)
    }
}
