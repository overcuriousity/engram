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
    /** The keys are kept before the PUT, so a failed PUT can be retried without the distributor's help. */
    suspend fun onEndpoint(endpoint: String, p256dh: String, auth: String, distributor: String) {
        store.pushKeys = PushKeys(endpoint, p256dh, auth, distributor)
        transportFor()?.registerPush(endpoint, p256dh, auth)
    }

    /** Re-send whatever is stored. For the retry worker and for a re-pair. */
    suspend fun resend() {
        val k = store.pushKeys ?: return
        transportFor()?.registerPush(k.endpoint, k.p256dh, k.auth)
    }

    suspend fun onUnregistered() {
        runCatching { transportFor()?.unregisterPush() }
        store.pushKeys = null
    }

    suspend fun received(bytes: ByteArray, decrypted: Boolean): Payload {
        if (!decrypted) return Payload.Unknown(null)
        val p = Payload.parse(bytes)
        if (p is Payload.Due && p.moments.isNotEmpty()) {
            val now = clock()
            db.momentsDao().upsert(p.moments.map { MomentRow(it.id, it.title, it.at, now) })
        }
        return p
    }
}
