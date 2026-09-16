package io.github.overcuriousity.engram.core

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** What the phone knows about its server. `null` in the store means unpaired. */
@Serializable
data class Connection(
    val origin: String,
    val token: String,
    /** SPKI SHA-256, base64url, or null when the chain was publicly trusted. */
    val pin: String?,
    val serverVersion: String,
    val deviceName: String,
)

/** The UnifiedPush registration as the server knows it. */
@Serializable
data class PushKeys(val endpoint: String, val p256dh: String, val auth: String, val distributor: String)

interface SecretBox {
    fun seal(plain: ByteArray): ByteArray
    fun open(sealed: ByteArray): ByteArray
}

/** Tests only: nothing sealed. */
class PlainBox : SecretBox {
    override fun seal(plain: ByteArray) = plain
    override fun open(sealed: ByteArray) = sealed
}

/**
 * AES-GCM under a key that never leaves the Keystore; the 12-byte IV is
 * prefixed. The same thing `EncryptedSharedPreferences` did before it was
 * deprecated, in thirty lines that will not be.
 */
class KeystoreBox(private val alias: String = "engram-connection") : SecretBox {
    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(alias, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return gen.generateKey()
    }

    override fun seal(plain: ByteArray): ByteArray {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key())
        return c.iv + c.doFinal(plain)
    }

    override fun open(sealed: ByteArray): ByteArray {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, sealed, 0, 12))
        return c.doFinal(sealed, 12, sealed.size - 12)
    }
}

@Serializable
private data class Stored(val connection: Connection? = null, val pushKeys: PushKeys? = null)

/**
 * One sealed file. Small enough to rewrite whole on every change, which is
 * what makes "set" and "clear" atomic: write beside, rename over.
 */
class ConnectionStore(private val file: File, private val box: SecretBox) {
    private val json = Json { ignoreUnknownKeys = true; encodeDefaults = true }
    private var stored: Stored = load()
    private val _current = MutableStateFlow(stored.connection)
    val current: StateFlow<Connection?> get() = _current

    var pushKeys: PushKeys?
        get() = stored.pushKeys
        set(v) { stored = stored.copy(pushKeys = v); save() }

    fun set(c: Connection) { stored = stored.copy(connection = c); save(); _current.value = c }

    fun clear() { stored = Stored(); save(); _current.value = null }

    private fun load(): Stored =
        if (!file.exists()) Stored()
        else runCatching { json.decodeFromString<Stored>(String(box.open(file.readBytes()))) }.getOrDefault(Stored())

    private fun save() {
        file.parentFile?.mkdirs()
        val tmp = File(file.path + ".tmp")
        tmp.writeBytes(box.seal(json.encodeToString(Stored.serializer(), stored).toByteArray()))
        tmp.renameTo(file)
    }
}
