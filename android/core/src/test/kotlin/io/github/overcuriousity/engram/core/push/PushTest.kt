package io.github.overcuriousity.engram.core.push

import androidx.test.core.app.ApplicationProvider
import io.github.overcuriousity.engram.core.Connection
import io.github.overcuriousity.engram.core.ConnectionStore
import io.github.overcuriousity.engram.core.PlainBox
import io.github.overcuriousity.engram.core.PushKeys
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Db
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.test.runTest
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.File
import kotlin.io.path.createTempDirectory

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class PushTest {
    private val server = MockWebServer()
    private val db = Db.inMemory(ApplicationProvider.getApplicationContext())
    private lateinit var store: ConnectionStore
    private lateinit var push: Push

    @Before fun up() {
        server.start()
        store = ConnectionStore(File(createTempDirectory("c").toFile(), "conn"), PlainBox())
        store.set(Connection(server.url("/").toString().trimEnd('/'), "tok", null, "1", "d"))
        push = Push(store, { store.current.value?.let { Transport(it, "ua") } }, db) { 42L }
    }
    @After fun down() = server.close()

    @Test fun anEndpointIsSentToTheServerAndRemembered() = runTest {
        server.enqueue(MockResponse(code = 204))
        push.onEndpoint("https://push.test/e", "BPkey", "authsecret", "org.example.dist")
        assertEquals("PUT", server.takeRequest().method)
        assertEquals(PushKeys("https://push.test/e", "BPkey", "authsecret", "org.example.dist"), store.pushKeys)
    }

    @Test fun aServerFailureKeepsTheKeysForRetry() = runTest {
        server.enqueue(MockResponse(code = 503))
        try { push.onEndpoint("https://push.test/e", "k", "a", "d"); fail() } catch (e: java.io.IOException) {}
        assertNotNull(store.pushKeys)
    }

    @Test fun unregisteredDeletesOnTheServerAndForgets() = runTest {
        store.pushKeys = PushKeys("e", "k", "a", "d")
        server.enqueue(MockResponse(code = 204))
        push.onUnregistered()
        assertEquals("DELETE", server.takeRequest().method)
        assertNull(store.pushKeys)
    }

    @Test fun aDuePayloadIsCached() = runTest {
        val p = push.received("""{"v":1,"kind":"due","at":1,"moments":[{"id":"m1","title":"Call","at":9}],"more":0}""".toByteArray(), decrypted = true)
        assertTrue(p is Payload.Due)
        val row = db.momentsDao().latest().first()!!
        assertEquals("m1", row.id); assertEquals(42L, row.fetchedAt)
    }

    @Test fun anUndecryptedMessageIsUnknown() = runTest {
        assertEquals(Payload.Unknown(null), push.received("""{"v":1}""".toByteArray(), decrypted = false))
    }
}
