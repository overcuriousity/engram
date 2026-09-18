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

/**
 * When a snooze lands, as the web's band computes it: `hour` is an hour from
 * now; `tomorrow` and `monday` are 09:00 on that day in `zone` — the hour the
 * server's own band uses. Unix seconds.
 */
fun snoozeUntil(word: String, nowMs: Long, zone: ZoneId): Long? {
    val now = Instant.ofEpochMilli(nowMs).atZone(zone)
    if (word == "hour") return now.toEpochSecond() + 3_600
    if (word != "tomorrow" && word != "monday") return null
    var d = now.toLocalDate().plusDays(1)
    if (word == "monday") while (d.dayOfWeek != java.time.DayOfWeek.MONDAY) d = d.plusDays(1)
    return d.atTime(9, 0).atZone(zone).toEpochSecond()
}

/** `Tue 22 Sep, 09:00`: the full date a countdown gives up, kept where a day is not losable. */
fun fullWhen(at: Long, zone: ZoneId): String =
    DateTimeFormatter.ofPattern("EEE d MMM, HH:mm", Locale.ENGLISH).format(Instant.ofEpochSecond(at).atZone(zone))

/**
 * The answer's `[n]` citations as links a tap can follow. Written as
 * markdown links to a `cite:` address, which the markdown view hands to the
 * screen instead of a browser. The parsed number, never the digits as
 * written: `[01]` cites excerpt one. A bracket naming no excerpt is left as
 * text — a link that scrolls nowhere reads as provenance the base cannot show.
 */
fun citeLinks(answer: String, n: Int): String =
    Regex("\\[(\\d{1,3})\\]").replace(answer) { m ->
        val i = m.groupValues[1].toIntOrNull()
        if (i != null && i in 1..n) "[\\[$i\\]](cite:$i)" else m.value
    }

/** `Thursday, 17 September 2026`. */
fun dayHeading(date: LocalDate): String =
    DateTimeFormatter.ofLocalizedDate(FormatStyle.FULL).withLocale(Locale.ENGLISH).format(date)

/** What is still owed. Only `queued`: a sent row is history and a held one is waiting on a person, not the network. */
fun queuedCount(rows: List<OutboxRow>): Int = rows.count { it.state == State.queued }

/** Whether the queue is worth a glance: something still owed, or something the server would not take. */
fun queueWorthAGlance(rows: List<OutboxRow>): Boolean = rows.any { it.state == State.queued || it.state == State.held || it.state == State.refused }

/** A line under the box, and whether it is bad news. */
data class CaptureWords(val text: String, val wrong: Boolean = false)

/**
 * What became of a capture, read off its outbox row. The line the web's
 * `_idle_foot` says — what was last kept — with the two states a phone has
 * that a browser does not: on its way, and refused.
 */
fun captureWords(row: OutboxRow?): CaptureWords = when {
    // Swept, or delivered and gone: kept.
    row == null || row.state == State.sent -> CaptureWords("Kept")
    row.state == State.held -> CaptureWords("Not kept · ${row.error ?: "the server refused it"} · see Queue", wrong = true)
    row.state == State.refused -> CaptureWords("Kept on the phone · unpaired on the server · see Queue", wrong = true)
    row.attempts > 0 -> CaptureWords("Kept on the phone · sent when the server can be reached")
    else -> CaptureWords("Keeping…")
}

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
