package io.github.overcuriousity.engram.ui

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import io.github.overcuriousity.engram.core.read.Disagreement
import io.github.overcuriousity.engram.core.read.Hit
import io.github.overcuriousity.engram.core.read.SetAsideAction
import io.github.overcuriousity.engram.core.read.SetAsideRow
import io.github.overcuriousity.engram.core.read.Reach
import io.github.overcuriousity.engram.core.read.Read
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * What is actually drawn, for the two things that must not be lost on the way
 * from a flag to a screen. `RailTest` proves the rule is *placed*; this proves
 * it is *there* — the named failure is a rewrite that drops it because it
 * looked like chrome, and that rewrite would pass every test of `railOf`.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class DrawnTest {
    @get:Rule val compose = createComposeRule()

    @Test fun theRuleIsOnScreenAndTheRowsBeneathItKeepTheirRank() {
        val hits = listOf(
            Hit("a", title = "An answer", text = "body a"),
            Hit("b", title = "Placed but not claimed", text = "body b", pastCliff = true),
            Hit("c", title = "Barely related", text = "body c", pastCliff = true, weak = true),
        )
        var opened = ""
        compose.setContent { EngramTheme { Rail(railOf(hits)) { opened = it } } }

        compose.onAllNodesWithText("Relevance falls off here").assertCountEquals(1)
        compose.onNodeWithText("#1").assertExists()
        compose.onNodeWithText("#2").assertExists()
        // The loose one says so in place of a rank.
        compose.onNodeWithText("#3").assertDoesNotExist()
        compose.onNodeWithText("loose").assertExists()

        compose.onNodeWithText("Placed but not claimed").performClick()
        assertEquals("b", opened)
    }

    @Test fun aListThatNeverFallsOffDrawsNoRule() {
        compose.setContent { EngramTheme { Rail(railOf(listOf(Hit("a", title = "One", text = "x")))) {} } }
        compose.onNodeWithText("Relevance falls off here").assertDoesNotExist()
    }

    @Test fun anUnreachableServerIsSaidOverWhatWasFetchedBefore() {
        var retried = 0
        val state = ReadState(Read("held from earlier", 1_000L, Reach.Unreachable, loading = false)) { retried++ }
        compose.setContent { EngramTheme { ReadFrame(state) { androidx.compose.material3.Text(it) } } }

        compose.onNodeWithText("Server unreachable", substring = true).assertExists()
        compose.onNodeWithText("fetched", substring = true).assertExists()
        compose.onNodeWithText("held from earlier").assertExists()
        compose.onNodeWithText("Retry").performClick()
        assertEquals(1, retried)
    }

    @Test fun andOverNothingWhenNothingWasFetched() {
        val state = ReadState(Read<String>(null, null, Reach.Unreachable, loading = false)) {}
        compose.setContent { EngramTheme { ReadFrame(state) { androidx.compose.material3.Text(it) } } }
        compose.onNodeWithText("Server unreachable").assertExists()
    }

    @Test fun aReachableServerSaysNothingAboutItself() {
        val state = ReadState(Read("fresh", 1_000L, Reach.Fresh, loading = false)) {}
        compose.setContent { EngramTheme { ReadFrame(state) { androidx.compose.material3.Text(it) } } }
        compose.onNodeWithText("Server unreachable", substring = true).assertDoesNotExist()
        compose.onNodeWithText("fresh").assertExists()
    }

    @Test fun aHitSaysWhichNoteDisagreesWithItAndOpensThatOne() {
        val hits = listOf(
            Hit(
                "a", title = "Backups", text = "kept for 14 days",
                disagreesWith = listOf(Disagreement("a", "b", otherTitle = "NAS", otherCreatedAt = 1_726_099_200, detail = "30 days there, 14 here")),
            ),
            Hit("c", title = "Untroubled", text = "nothing disagrees with this"),
        )
        var opened = ""
        compose.setContent { EngramTheme { Rail(railOf(hits)) { opened = it } } }

        compose.onNodeWithText("Disagrees with NAS: 30 days there, 14 here").assertExists()
        compose.onAllNodesWithText("Disagrees with", substring = true).assertCountEquals(1)
        compose.onNodeWithText("Disagrees with NAS: 30 days there, 14 here").performClick()
        assertEquals("the line opens the other note, not this one", "b", opened)
    }

    @Test fun aDisagreementWithNoTitleOrDetailStillSaysThereIsOne() {
        assertEquals("Disagrees with another note", disagreementWords(Disagreement("a", "b")))
        assertEquals("Disagrees with NAS", disagreementWords(Disagreement("a", "b", otherTitle = "NAS")))
    }

    /**
     * One artifact can be under two kinds of the journal at once — one the
     * base wrote and later hid is a `generated` row and a `hidden` one — and
     * they take different answers. Keyed by the subject alone, answering
     * either made both rows disappear, the second having been enqueued for
     * nothing.
     */
    @Test fun oneArtifactUnderTwoKindsIsTwoRowsAndAnsweringOneLeavesTheOther() {
        val rows = listOf(
            SetAsideRow(kind = "hidden", subjectId = "a1", artifactId = "a1", label = "Clinic hours", named = true, why = "near-identical to the one beside it, so it is kept out of results"),
            SetAsideRow(kind = "generated", subjectId = "a1", artifactId = "a1", label = "Clinic hours", named = true, why = "written after a run of searches the base could not answer"),
        )
        val enqueued = mutableListOf<SetAsideAction>()
        compose.setContent {
            EngramTheme {
                Journal(rows, capped = false, onAction = { _, a -> enqueued += a; "row-1" }, onUndo = { true }, onOpen = {})
            }
        }

        compose.onNodeWithText("Return to results").performClick()
        compose.waitForIdle()

        assertEquals(listOf(SetAsideAction.Reactivate), enqueued)
        compose.onNodeWithText("written after a run of searches the base could not answer").assertExists()
        compose.onAllNodesWithText("near-identical to the one beside it, so it is kept out of results").assertCountEquals(0)
    }

    @Test fun aSetAsideKindThisBuildHasNeverHeardOfDrawsNoButtons() {
        val rows = listOf(
            SetAsideRow(kind = "merged", subjectId = "m1", artifactId = "m1", label = "Merged, shorter.", named = true, why = "written from 2 artifacts"),
            SetAsideRow(kind = "something-the-server-grew", subjectId = "x1", artifactId = "x1", label = "A newer sort of row", named = true, why = "the base did something this app cannot answer"),
        )
        compose.setContent {
            EngramTheme { Journal(rows, capped = false, onAction = { _, _ -> "row-1" }, onUndo = { true }, onOpen = {}) }
        }
        // The one it knows keeps its answer; the one it does not is still shown.
        // Named for what it undoes: "Undo" alone is what the bar over an answer
        // of one's own says, and this undoes something the base did.
        compose.onNodeWithText("Undo the merge").assertExists()
        compose.onNodeWithText("A newer sort of row").assertExists()
        compose.onNodeWithText("the base did something this app cannot answer").assertExists()
        compose.onAllNodesWithText("Open").assertCountEquals(2)
    }
}
