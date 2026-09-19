package io.github.overcuriousity.engram.push

import android.app.AlarmManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import io.github.overcuriousity.engram.App
import io.github.overcuriousity.engram.core.push.Moment
import io.github.overcuriousity.engram.core.push.Payload
import io.github.overcuriousity.engram.core.reminders.Ring
import kotlinx.coroutines.launch

/**
 * The alarms behind a contained phone's reminders. Inexact on purpose:
 * `setAndAllowWhileIdle` needs no permission a person has to grant, and a
 * reminder that rings a few minutes late in doze is still a reminder.
 */
object Alarms {
    private const val EXTRA_MOMENT = "moment"

    private fun intent(context: Context, id: String): PendingIntent = PendingIntent.getBroadcast(
        context, id.hashCode(), Intent(context, AlarmReceiver::class.java).putExtra(EXTRA_MOMENT, id),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    /** Make the alarms exactly these. Keyed by moment, so a re-set replaces, and what is no longer due is cancelled. */
    fun set(context: Context, rings: List<Ring>) {
        val am = context.getSystemService(AlarmManager::class.java)
        val prefs = context.getSharedPreferences("alarms", Context.MODE_PRIVATE)
        val before = prefs.getStringSet("set", emptySet()).orEmpty()
        val now = rings.map { it.id }.toSet()
        (before - now).forEach { am.cancel(intent(context, it)) }
        rings.forEach { am.setAndAllowWhileIdle(AlarmManager.RTC_WAKEUP, it.at * 1000, intent(context, it.id)) }
        prefs.edit().putStringSet("set", now).apply()
    }

    internal fun momentOf(intent: Intent): String? = intent.getStringExtra(EXTRA_MOMENT)
}

/** An alarm went off: show what was written down for it. The core is not needed, and is not started. */
class AlarmReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val id = Alarms.momentOf(intent) ?: return
        val app = context.applicationContext as App
        val pending = goAsync()
        app.scope.launch {
            try {
                val row = app.engram.reminder(id) ?: return@launch
                app.engram.rung = app.engram.rung + id
                // The same notification a server's push draws, with the same
                // Done and Snooze: those go through the outbox to the core.
                Reminders.show(context, Payload.Due(row.at, listOf(Moment(row.id, row.title, row.at)), 0))
            } finally {
                pending.finish()
            }
        }
    }
}

/** Alarms do not survive a reboot. What was written down does, so they are set again from it. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED) return
        val app = context.applicationContext as App
        if (!app.engram.loopback) return
        val pending = goAsync()
        app.scope.launch {
            try {
                Alarms.set(context, app.engram.reminders())
            } finally {
                pending.finish()
            }
        }
    }
}
