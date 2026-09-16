package io.github.overcuriousity.engram.push

import android.app.NotificationManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import io.github.overcuriousity.engram.App
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.launch

/** Done and snooze are writes the device owes the server: outbox rows, like any capture. */
class ActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val app = context.applicationContext as App
        val moment = intent.getStringExtra("moment") ?: return
        val pending = goAsync()
        app.scope.launch {
            try {
                when (intent.action) {
                    Reminders.ACTION_DONE -> app.engram.outbox.enqueueDone(moment)
                    Reminders.ACTION_SNOOZE -> app.engram.outbox.enqueueSnooze(moment, System.currentTimeMillis() / 1000 + 3600)
                }
                Sync.kick(context)
                context.getSystemService(NotificationManager::class.java).cancelAll()
            } finally {
                pending.finish()
            }
        }
    }
}
