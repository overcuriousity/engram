package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.db.OutboxRow
import io.github.overcuriousity.engram.core.db.State
import io.github.overcuriousity.engram.core.read.Offer
import java.time.Instant
import java.time.LocalDate
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.time.format.FormatStyle
import java.util.Locale

/* Times and counts, in words. Pure, so the zone and the clock are arguments. */

private val HM = DateTimeFormatter.ofPattern("HH:mm")
private val DAY_MONTH = DateTimeFormatter.ofPattern("d MMM", Locale.ENGLISH)

/** `14:32` in `zone`. `at` is Unix seconds, as everything from the API is. */
fun clock(at: Long, zone: ZoneId): String = HM.format(Instant.ofEpochSecond(at).atZone(zone))

/** `12 Sep`, or `12 Sep 2025` in another year than `now`. */
fun dayWords(at: Long, nowMs: Long, zone: ZoneId): String {
    val d = Instant.ofEpochSecond(at).atZone(zone).toLocalDate()
    val today = Instant.ofEpochMilli(nowMs).atZone(zone).toLocalDate()
    return if (d.year == today.year) DAY_MONTH.format(d) else "${DAY_MONTH.format(d)} ${d.year}"
}

/** When what is on screen was last known current: `fetched 14:32` today, `fetched 12 Sep` before. */
fun fetchedWords(fetchedAtMs: Long, nowMs: Long, zone: ZoneId): String {
    val then = Instant.ofEpochMilli(fetchedAtMs).atZone(zone)
    val today = Instant.ofEpochMilli(nowMs).atZone(zone).toLocalDate()
    return if (then.toLocalDate() == today) "fetched ${HM.format(then)}" else "fetched ${dayWords(fetchedAtMs / 1000, nowMs, zone)}"
}

/** `in 2 h`, `in 3 d`, `overdue`, or nothing for a reminder with no time. */
fun dueWords(at: Long?, nowMs: Long): String {
    if (at == null) return ""
    val s = at - nowMs / 1000
    return when {
        s < 0 -> "overdue"
        s < 3_600 -> "in ${maxOf(1, s / 60)} min"
        s < 86_400 -> "in ${s / 3_600} h"
        else -> "in ${s / 86_400} d"
    }
}

/** `Thursday, 17 September 2026`. */
fun dayHeading(date: LocalDate): String =
    DateTimeFormatter.ofLocalizedDate(FormatStyle.FULL).withLocale(Locale.ENGLISH).format(date)

/** What is still owed. Only `queued`: a sent row is history and a held one is waiting on a person, not the network. */
fun queuedCount(rows: List<OutboxRow>): Int = rows.count { it.state == State.queued }

/**
 * The line under the offer card, from the ladder's own word. A client owns its
 * wording; the rung is what the server sends and what `seen` sends back.
 * `random` claims nothing, because nothing about the situation produced it.
 */
fun offerLine(o: Offer, zone: ZoneId): String = when (o.rung) {
    "pattern" -> o.at?.let {
        val z = o.atTz?.let { tz -> runCatching { ZoneId.of(tz) }.getOrNull() } ?: zone
        val t = Instant.ofEpochSecond(it).atZone(z)
        "like ${DateTimeFormatter.ofPattern("EEE HH:mm", Locale.ENGLISH).format(t)}"
    } ?: "a pattern"
    "similar" -> "like what you opened"
    "tentative" -> if (o.events <= 1) "once before" else "${o.events} times before"
    else -> ""
}
