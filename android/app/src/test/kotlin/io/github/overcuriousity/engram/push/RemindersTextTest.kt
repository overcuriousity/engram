package io.github.overcuriousity.engram.push

import io.github.overcuriousity.engram.core.push.Moment
import io.github.overcuriousity.engram.core.push.Payload
import org.junit.Assert.assertEquals
import org.junit.Test

class RemindersTextTest {
    @Test fun dueListsTheMomentsAndCountsTheRest() {
        val (title, body) = Reminders.lines(Payload.Due(0, listOf(Moment("a", "Call Sam", 0), Moment("b", "Pay rent", 0)), 3))
        assertEquals("Due", title)
        assertEquals("Call Sam\nPay rent\n+3 more", body)
    }

    @Test fun aSingleMomentIsTheTitle() {
        assertEquals("Call Sam" to "", Reminders.lines(Payload.Due(0, listOf(Moment("a", "Call Sam", 0)), 0)))
    }

    @Test fun noticeIsItself() {
        assertEquals("Test" to "It works", Reminders.lines(Payload.Notice(0, "Test", "It works")))
    }

    @Test fun unknownStillRings() {
        assertEquals("Something is due" to "This app is behind the server · update it", Reminders.lines(Payload.Unknown(2)))
        assertEquals("Something is due" to "This app is behind the server · update it", Reminders.lines(Payload.Unknown(null)))
    }
}
