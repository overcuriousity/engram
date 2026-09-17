package io.github.overcuriousity.engram.push

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import io.github.overcuriousity.engram.MainActivity
import io.github.overcuriousity.engram.R
import io.github.overcuriousity.engram.core.push.Payload

object Reminders {
    const val CHANNEL = "reminders"
    const val ACTION_DONE = "io.github.overcuriousity.engram.DONE"
    const val ACTION_SNOOZE = "io.github.overcuriousity.engram.SNOOZE"
    const val EXTRA_NOTIFICATION = "notification"

    fun ensureChannel(context: Context) {
        val nm = context.getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "Reminders", NotificationManager.IMPORTANCE_HIGH))
    }

    /** Title and body. A version this app does not know still rings, and says why it says so little. */
    fun lines(p: Payload): Pair<String, String> = when (p) {
        is Payload.Due -> when {
            p.moments.size == 1 && p.more == 0 -> p.moments[0].title to ""
            else -> "Due" to (p.moments.map { it.title } + (if (p.more > 0) listOf("+${p.more} more") else emptyList())).joinToString("\n")
        }
        is Payload.Notice -> p.title to p.body
        is Payload.Unknown -> "Something is due" to "This app is behind the server · update it"
    }

    fun show(context: Context, p: Payload) {
        val (title, body) = lines(p)
        val open = PendingIntent.getActivity(context, 0, Intent(context, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
        val b = NotificationCompat.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.ic_tile)
            .setContentTitle(title)
            .setStyle(NotificationCompat.BigTextStyle().bigText(body))
            .setContentText(body.lineSequence().firstOrNull() ?: "")
            .setContentIntent(open)
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
        val id = notificationId(p)
        if (p is Payload.Due && p.moments.isNotEmpty()) {
            val first = p.moments[0]
            b.addAction(0, "Done", action(context, ACTION_DONE, first.id, id))
            b.addAction(0, "Snooze 1 h", action(context, ACTION_SNOOZE, first.id, id))
        }
        context.getSystemService(NotificationManager::class.java).notify(id, b.build())
    }

    /**
     * The action carries the notification it came from, so settling one
     * moment dismisses that notification and no other. Several rungs can be
     * on the shade at once, each about a different moment.
     */
    private fun action(context: Context, action: String, moment: String, notification: Int): PendingIntent =
        PendingIntent.getBroadcast(
            context, moment.hashCode(),
            Intent(context, ActionReceiver::class.java).setAction(action)
                .putExtra("moment", moment)
                .putExtra(EXTRA_NOTIFICATION, notification),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )

    /** One notification per push, keyed by its instant, so a ladder rung replaces the previous one. */
    private fun notificationId(p: Payload) = when (p) {
        is Payload.Due -> (p.at % Int.MAX_VALUE).toInt()
        is Payload.Notice -> (p.at % Int.MAX_VALUE).toInt()
        else -> 1
    }
}
