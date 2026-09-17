package io.github.overcuriousity.engram.push

import io.github.overcuriousity.engram.App
import kotlinx.coroutines.launch
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.PushService
import org.unifiedpush.android.connector.UnifiedPush
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage

/**
 * Every callback here is delivered on the main thread, so none of them may
 * wait on the network or the database. What the app owes the server is
 * written down and handed to a worker; what the person must see is drawn at
 * once from the bytes themselves.
 */
class PushServiceImpl : PushService() {
    private val app get() = application as App
    private val engram get() = app.engram

    override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) {
        // No keys, no encryption: the server would keep a legacy plaintext
        // row, which the app must never be the one to create.
        val keys = endpoint.pubKeySet ?: return
        val distributor = UnifiedPush.getAckDistributor(this) ?: "?"
        // Record, then let the worker do the PUT. Doing it here blocked the
        // main thread for as long as the server took to answer — up to the
        // client's minute of read timeout, which is an ANR.
        engram.push.rememberKeys(endpoint.url, keys.pubKey, keys.auth, distributor)
        PushRetry.schedule(this)
    }

    override fun onMessage(message: PushMessage, instance: String) {
        // Decoding is arithmetic; the notification goes up immediately, and
        // the band's copy of the moments is written behind it.
        val payload = engram.push.decode(message.content, message.decrypted)
        Reminders.show(this, payload)
        app.scope.launch { engram.push.rememberMoments(payload) }
    }

    override fun onRegistrationFailed(reason: FailedReason, instance: String) {
        engram.pushFailure.value = reason.name
    }

    override fun onUnregistered(instance: String) {
        // Dropped locally at once — that much is true whatever the network
        // says — and the server is told behind it.
        engram.push.forgetKeys()
        app.scope.launch { engram.push.tellUnregistered() }
    }
}
