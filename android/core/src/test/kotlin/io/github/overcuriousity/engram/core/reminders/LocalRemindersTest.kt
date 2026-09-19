package io.github.overcuriousity.engram.core.reminders

import io.github.overcuriousity.engram.core.read.DueRow
import io.github.overcuriousity.engram.core.read.Moment
import org.junit.Assert.assertEquals
import org.junit.Test

class LocalRemindersTest {
    private val now = 1_000_000L
    private fun row(id: String, at: Long?, snoozed: Long? = null, title: String = id, opening: String = "") =
        DueRow(Moment(id, "a-$id", at = at, snoozedUntil = snoozed), title, named = title.isNotBlank(), opening = opening)

    @Test fun anUndatedRowIsAListEntryAndRingsNothing() =
        assertEquals(emptyList<Ring>(), LocalReminders.plan(listOf(row("a", null)), now, emptySet()))

    @Test fun aDatedRowRingsAtItsTimeAndASnoozedOneAtItsSnooze() = assertEquals(
        listOf(Ring("b", "b", now + 60), Ring("a", "a", now + 3600)),
        LocalReminders.plan(listOf(row("a", now - 10, snoozed = now + 3600), row("b", now + 60)), now, emptySet()),
    )

    @Test fun aMissedOneRingsNowAndOnlyOnce() {
        val missed = listOf(row("a", now - 500))
        assertEquals(listOf(Ring("a", "a", now)), LocalReminders.plan(missed, now, emptySet()))
        assertEquals(emptyList<Ring>(), LocalReminders.plan(missed, now, setOf("a")))
    }

    @Test fun oneThatRangAndWasSnoozedRingsAgain() = assertEquals(
        listOf(Ring("a", "a", now + 3600)),
        LocalReminders.plan(listOf(row("a", now - 500, snoozed = now + 3600)), now, setOf("a")),
    )

    @Test fun aNamelessRowRingsUnderItsOwnOpening() = assertEquals(
        listOf(Ring("a", "call the landlord about", now + 1)),
        LocalReminders.plan(listOf(row("a", now + 1, title = "", opening = "call the landlord about")), now, emptySet()),
    )
}
