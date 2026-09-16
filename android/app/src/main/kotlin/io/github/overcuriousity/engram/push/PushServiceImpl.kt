package io.github.overcuriousity.engram.push

import io.github.overcuriousity.engram.App
import kotlinx.coroutines.runBlocking
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.PushService
import org.unifiedpush.android.connector.UnifiedPush
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage

class PushServiceImpl : PushService() {
    private val engram get() = (application as App).engram

    override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) {
        // No keys, no encryption: the server would keep a legacy plaintext
        // row, which the app must never be the one to create.
        val keys = endpoint.pubKeySet ?: return
        val distributor = UnifiedPush.getAckDistributor(this) ?: "?"
        val landed = runBlocking { runCatching { engram.push.onEndpoint(endpoint.url, keys.pubKey, keys.auth, distributor) }.isSuccess }
        if (!landed) PushRetry.schedule(this)
    }

    override fun onMessage(message: PushMessage, instance: String) {
        val payload = runBlocking { engram.push.received(message.content, message.decrypted) }
        Reminders.show(this, payload)
    }

    override fun onRegistrationFailed(reason: FailedReason, instance: String) {
        engram.pushFailure.value = reason.name
    }

    override fun onUnregistered(instance: String) {
        runBlocking { engram.push.onUnregistered() }
    }
}
