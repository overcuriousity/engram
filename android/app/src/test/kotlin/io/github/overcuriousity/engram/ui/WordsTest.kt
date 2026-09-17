package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.OutboxRow
import io.github.overcuriousity.engram.core.db.State
import io.github.overcuriousity.engram.core.read.Offer
import org.junit.Assert.*
import org.junit.Test
import java.time.LocalDate
import java.time.ZoneId
import java.time.ZonedDateTime

class WordsTest {
    private val berlin = ZoneId.of("Europe/Berlin")
    private fun at(y: Int, m: Int, d: Int, h: Int, min: Int) = ZonedDateTime.of(y, m, d, h, min, 0, 0, berlin).toEpochSecond()
    private val now = at(2026, 9, 17, 18, 0) * 1000

    @Test fun fetchedTodayIsAClockTimeAndEarlierIsADate() {
        assertEquals("fetched 14:32", fetchedWords(at(2026, 9, 17, 14, 32) * 1000, now, berlin))
        assertEquals("fetched 12 Sep", fetchedWords(at(2026, 9, 12, 9, 0) * 1000, now, berlin))
        assertEquals("fetched 30 Dec 2025", fetchedWords(at(2025, 12, 30, 9, 0) * 1000, now, berlin))
    }

    @Test fun aClockIsReadInTheZoneItIsGiven() {
        assertEquals("14:32", clock(at(2026, 9, 17, 14, 32), berlin))
        assertEquals("12:32", clock(at(2026, 9, 17, 14, 32), ZoneId.of("UTC")))
    }

    @Test fun dueIsSaidInTheLargestUnitThatFits() {
        val s = now / 1000
        assertEquals("overdue", dueWords(s - 5, now))
        assertEquals("in 1 min", dueWords(s + 20, now))
        assertEquals("in 45 min", dueWords(s + 45 * 60, now))
        assertEquals("in 2 h", dueWords(s + 2 * 3_600 + 5, now))
        assertEquals("in 3 d", dueWords(s + 3 * 86_400 + 5, now))
        assertEquals("", dueWords(null, now))
    }

    @Test fun aDayIsHeadedInFull() = assertEquals("Thursday, September 17, 2026", dayHeading(LocalDate.of(2026, 9, 17)))

    @Test fun onlyWhatIsStillOwedIsCounted() {
        fun row(id: String, s: State) = OutboxRow(id, Kind.capture_text, "{}", 0, nextAt = 0, state = s)
        assertEquals(2, queuedCount(listOf(row("a", State.queued), row("b", State.sent), row("c", State.held), row("d", State.queued))))
    }

    @Test fun theOffersLineIsBuiltFromItsRungAndRandomClaimsNothing() {
        val o = Offer("a", rung = "pattern", at = at(2026, 9, 15, 20, 36), atTz = "Europe/Berlin")
        assertEquals("like Tue 20:36", offerLine(o, ZoneId.of("UTC")))
        assertEquals("like what you opened", offerLine(o.copy(rung = "similar"), berlin))
        assertEquals("once before", offerLine(o.copy(rung = "tentative", events = 1), berlin))
        assertEquals("4 times before", offerLine(o.copy(rung = "tentative", events = 4), berlin))
        assertEquals("", offerLine(o.copy(rung = "random"), berlin))
        assertEquals("", offerLine(o.copy(rung = "a rung from next year"), berlin))
    }
}
