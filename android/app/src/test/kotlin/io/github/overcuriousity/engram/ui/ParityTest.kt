package io.github.overcuriousity.engram.ui

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import io.github.overcuriousity.engram.core.read.AskAnswer
import io.github.overcuriousity.engram.core.read.Hit
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.time.DayOfWeek
import java.time.Instant
import java.time.ZoneId

/**
 * The pieces the web's pane has that this app grew to match: the two verdict
 * bars, the answer's badges and citation links, the snooze instants, and what
 * a rail row says about where its document goes on. Each is a rule that can
 * be checked without a server.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class ParityTest {
    @get:Rule val compose = createComposeRule()

    @Test fun theSearchBarOffersThreeAnswersThenAnUndoAndNeverAnUndoOnASkip() {
        var said: String? = null
        // The content is set once and the state walked through it: a test may
        // call setContent only the once, and the bar is a function of its verdict.
        var v by mutableStateOf(SearchVerdict())
        compose.setContent { EngramTheme { SearchVerdictBar(v) { said = it } } }
        compose.onNodeWithText("Was this what you were looking for?").assertIsDisplayed()
        compose.onNodeWithText("Not sure").performClick()
        assertEquals("skip", said)

        v = SearchVerdict(state = "hit")
        compose.onNodeWithText("yes, this was it").assertIsDisplayed()
        compose.onNodeWithText("Undo").performClick()
        assertEquals("none", said)

        v = SearchVerdict(state = "skip")
        compose.onNodeWithText("left unanswered").assertIsDisplayed()
        compose.onNodeWithText("Undo").assertDoesNotExist()

        v = SearchVerdict(already = true)
        compose.onNodeWithText("nothing to record — that search was already judged.").assertIsDisplayed()
    }

    @Test fun theAskBarSaysWhatWasJudgedInTheServersWords() {
        var said: String? = null
        var v by mutableStateOf(AskVerdict())
        compose.setContent { EngramTheme { AskVerdictBar(v) { said = it } } }
        compose.onNodeWithText("Nothing here").performClick()
        assertEquals("nothing_here", said)
        v = AskVerdict(verdict = "nothing here")
        compose.onNodeWithText("judged nothing here").assertIsDisplayed()
        compose.onNodeWithText("Undo").performClick()
        assertEquals("none", said)
    }

    @Test fun anAnswersBadgesAreTheWebsFacts() {
        assertEquals(emptyList<String>(), answerBadges(AskAnswer(answer = "a")))
        assertEquals(
            listOf("nothing here", "cut off at the answer length limit", "2 literals no excerpt supports", "written only from retired notes", "1 more excerpt did not fit"),
            answerBadges(AskAnswer(answer = "a", abstained = true, truncated = true, unsupported = listOf("x", "y"), retiredOnly = true, dropped = 1)),
        )
    }

    @Test fun citationsBecomeLinksOnlyWhereTheyNameAnExcerpt() {
        assertEquals("see [\\[1\\]](cite:1) and [\\[2\\]](cite:2)", citeLinks("see [1] and [2]", 2))
        assertEquals("[\\[1\\]](cite:1)", citeLinks("[01]", 3))
        assertEquals("see [7] and [a]", citeLinks("see [7] and [a]", 2))
    }

    @Test fun snoozesLandWhereTheWebsDo() {
        val zone = ZoneId.of("Europe/Berlin")
        // A Wednesday, 15:30 in Berlin.
        val now = Instant.parse("2026-09-16T13:30:00Z").toEpochMilli()
        assertEquals(now / 1000 + 3600, snoozeUntil("hour", now, zone))
        val tomorrow = Instant.ofEpochSecond(snoozeUntil("tomorrow", now, zone)!!).atZone(zone)
        assertEquals(17, tomorrow.dayOfMonth); assertEquals(9, tomorrow.hour); assertEquals(0, tomorrow.minute)
        val monday = Instant.ofEpochSecond(snoozeUntil("monday", now, zone)!!).atZone(zone)
        assertEquals(DayOfWeek.MONDAY, monday.dayOfWeek); assertEquals(21, monday.dayOfMonth); assertEquals(9, monday.hour)
        assertNull(snoozeUntil("never", now, zone))
    }

    @Test fun aRowSaysWhereItsDocumentGoesOnByRankOrByOffer() {
        val a = Hit("a", continuesTo = "b")
        val b = Hit("b", continuesTo = "z")
        val rail = railOf(listOf(a, b))
        assertEquals("↓ continues in #2", continuesWords(a, rail))
        assertEquals("↳ continues in the next passage", continuesWords(b, rail))
        assertNull(continuesWords(Hit("c"), rail))
        // A borrowed heading is where the passage sits, not what it is called.
        assertEquals("Kapitel 3", sectionOf(Hit("d", title = "Kapitel 3", borrowedName = true, text = "Der Vorgang")))
        assertNull(sectionOf(Hit("e", title = "Kapitel 3", borrowedName = true, text = "Kapitel 3 beginnt")))
        assertNull(sectionOf(Hit("f", title = "A name", text = "x")))
        assertEquals("a loose match · written by a model from 2 sources", whyOf(Hit("g", weak = true, modelWritten = true, originCount = 2), allLoose = false))
        assertNull(whyOf(Hit("h"), allLoose = false))
    }
}
