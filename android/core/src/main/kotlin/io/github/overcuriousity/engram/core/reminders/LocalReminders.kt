package io.github.overcuriousity.engram.core.reminders

import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.db.MomentRow
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.DueRow

/** One reminder to ring: which, under what words, and when, in epoch seconds. */
data class Ring(val id: String, val title: String, val at: Long)

/**
 * Reminders on a phone that is its own engram. No server pushes them, so the
 * phone reads what the core says is due, writes it down, and sets its own
 * alarms. Written down so that an alarm can ring, and a reboot can re-set
 * them, without the core being up.
 */
object LocalReminders {
    /**
     * Who sets the alarms. The receivers and the notification are the app's,
     * and this module cannot name them; the app says who at start.
     */
    @Volatile var ringer: ((android.content.Context, List<Ring>) -> Unit)? = null

    /** [sync], and the alarms made to match. What every caller wants. */
    suspend fun refresh(engram: Engram) {
        sync(engram)?.let { rings -> ringer?.invoke(engram.app, rings) }
    }

    /** How far ahead alarms are set. The next sync sets the ones beyond it. */
    const val AHEAD_SECS = 30L * 86_400

    /**
     * What to ring. A dated row rings at its time, or at its snooze. One whose
     * time has passed — the phone was off, the alarm was lost — rings now, once:
     * [rung] is what has already rung, and only a later time rings it again,
     * which is exactly what a snooze is. An undated row is a list entry and
     * rings nothing.
     */
    fun plan(rows: List<DueRow>, now: Long, rung: Set<String>): List<Ring> = rows.mapNotNull { r ->
        val t = r.at ?: return@mapNotNull null
        val title = r.title.ifBlank { r.opening }
        when {
            t > now -> Ring(r.moment.id, title, t)
            r.moment.id !in rung -> Ring(r.moment.id, title, now)
            else -> null
        }
    }.sortedBy { it.at }

    /**
     * Read what is due from the core, keep it, and answer what to ring. Null
     * where there is nothing to ask — server mode, or a core that is not up —
     * which is different from an empty list: that one means cancel everything.
     */
    suspend fun sync(engram: Engram, now: Long = System.currentTimeMillis() / 1000): List<Ring>? {
        if (!engram.loopback) return null
        val rows = engram.dueUntil(now + AHEAD_SECS) ?: return null
        val rings = plan(rows, now, engram.rung)
        val dao = engram.db.momentsDao()
        dao.upsert(rings.map { MomentRow(it.id, it.title, it.at, now) })
        dao.deleteExcept(rings.map { it.id })
        engram.rung = engram.rung intersect rows.map { it.moment.id }.toSet()
        return rings
    }
}
