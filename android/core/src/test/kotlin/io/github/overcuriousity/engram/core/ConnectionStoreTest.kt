package io.github.overcuriousity.engram.core

import org.junit.Assert.*
import org.junit.Test
import java.io.File
import kotlin.io.path.createTempDirectory

class ConnectionStoreTest {
    private val dir = createTempDirectory("engram").toFile()
    private fun store() = ConnectionStore(File(dir, "connection"), PlainBox())
    private val c = Connection("https://engram.test", "engram_abc", null, "0.1.0", "engram for Android 0.1.0 · Pixel 8")

    @Test fun startsUnpaired() { assertNull(store().current.value) }

    @Test fun setThenReadBackAcrossInstances() {
        store().set(c)
        assertEquals(c, store().current.value)
    }

    @Test fun clearForgetsTheConnectionAndTheKeys() {
        val s = store()
        s.set(c)
        s.pushKeys = PushKeys("https://push.test/x", "BP…", "auth", "org.example.distributor")
        s.clear()
        assertNull(store().current.value)
        assertNull(store().pushKeys)
    }

    @Test fun theFileIsNotPlaintextWhenTheBoxSeals() {
        val s = ConnectionStore(File(dir, "sealed"), object : SecretBox {
            override fun seal(plain: ByteArray) = plain.map { (it.toInt() xor 0x5a).toByte() }.toByteArray()
            override fun open(sealed: ByteArray) = seal(sealed)
        })
        s.set(c)
        assertFalse(File(dir, "sealed").readText(Charsets.ISO_8859_1).contains("engram_abc"))
    }
}
