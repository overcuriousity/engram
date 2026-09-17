package io.github.overcuriousity.engram.push

import android.content.Context
import io.github.overcuriousity.engram.App
import io.github.overcuriousity.engram.core.Engram
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.unifiedpush.android.connector.UnifiedPush

/**
 * Pick a distributor and register. The VAPID key is fetched first so the
 * distributor can hand the endpoint a key the server will sign with.
 */
object PushRegistrar {
    fun distributors(context: Context): List<String> = UnifiedPush.getDistributors(context)

    fun ensure(context: Context, engram: Engram, onNoDistributor: () -> Unit) {
        if (UnifiedPush.getDistributors(context).isEmpty()) {
            onNoDistributor()
            return
        }
        (engram.app as App).scope.launch {
            val vapid = runCatching { engram.vapid() }.getOrNull()
            withContext(Dispatchers.Main) {
                UnifiedPush.tryUseCurrentOrDefaultDistributor(context) { ok ->
                    if (ok) UnifiedPush.register(context, vapid = vapid)
                }
            }
        }
    }

    fun forget(context: Context) = UnifiedPush.unregister(context)
}
