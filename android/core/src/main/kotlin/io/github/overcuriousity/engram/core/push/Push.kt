package io.github.overcuriousity.engram.core.push

import io.github.overcuriousity.engram.core.ConnectionStore
import io.github.overcuriousity.engram.core.PushKeys
import io.github.overcuriousity.engram.core.Transport
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.MomentRow

/**
 * The server side of a UnifiedPush registration. The connector owns the
 * distributor and the keys, and decrypts what arrives; this owns telling the
 * server and reading what came.
 */
class Push internal constructor(
    private val store: ConnectionStore,
    private val transportFor: () -> Transport?,
    private val db: Db,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    /**
     * Write the registration down and nothing else. The distributor hands the
     * endpoint over on the main thread, where a PUT cannot go; recording it
     * is a few hundred bytes to a local file, and the worker owes the server
     * the rest. Stored before any send, so a failed PUT is retried without
     * the distributor's help.
     */
    fun rememberKeys(endpoint: String, p256dh: String, auth: String, distributor: String) {
        store.pushKeys = PushKeys(endpoint, p256dh, auth, distributor)
    }

    /** The keys are kept before the PUT, so a failed PUT can be retried without the distributor's help. */
    suspend fun onEndpoint(endpoint: String, p256dh: String, auth: String, distributor: String) {
        rememberKeys(endpoint, p256dh, auth, distributor)
        transportFor()?.registerPush(endpoint, p256dh, auth)
    }

    /** Re-send whatever is stored. For the retry worker and for a re-pair. */
    suspend fun resend() {
        val k = store.pushKeys ?: return
        transportFor()?.registerPush(k.endpoint, k.p256dh, k.auth)
    }

    /** Drop the registration locally. The server is told separately, and best-effort. */
    fun forgetKeys() {
        store.pushKeys = null
    }

    /** Tell the server the registration is gone. Best-effort; the local drop stands either way. */
    suspend fun tellUnregistered() {
        runCatching { transportFor()?.unregisterPush() }
    }

    suspend fun onUnregistered() {
        tellUnregistered()
        forgetKeys()
    }

    /**
     * What arrived, without touching the database. Pure, so the notification
     * can be drawn on the thread the message was delivered on and the moments
     * written afterwards.
     */
    fun decode(bytes: ByteArray, decrypted: Boolean): Payload =
        if (!decrypted) Payload.Unknown(null) else Payload.parse(bytes)

    /** The band's copy of what the push said. Nothing to write for any other kind. */
    suspend fun rememberMoments(p: Payload) {
        if (p is Payload.Due && p.moments.isNotEmpty()) {
            val now = clock()
            db.momentsDao().upsert(p.moments.map { MomentRow(it.id, it.title, it.at, now) })
        }
    }

    suspend fun received(bytes: ByteArray, decrypted: Boolean): Payload =
        decode(bytes, decrypted).also { rememberMoments(it) }
}
