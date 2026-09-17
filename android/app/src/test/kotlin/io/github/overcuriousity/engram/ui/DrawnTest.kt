package io.github.overcuriousity.engram.ui

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.runtime.Composable
import io.github.overcuriousity.engram.core.read.Hit
import io.github.overcuriousity.engram.core.read.Pair
import io.github.overcuriousity.engram.core.read.PairSide
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

    private fun pair(
        id: Long = 1,
        percent: Long = 91,
        viaLink: Boolean = false,
        finding: String? = null,
        unjudged: Boolean = false,
        mergeable: Boolean = true,
    ) = Pair(
        id = id,
        percent = percent,
        viaLink = viaLink,
        a = PairSide("art-a", "Timeout 0", named = true, excerpt = "the timeout is 30 seconds"),
        b = PairSide("art-b", "Timeout 1", named = true, excerpt = "the timeout is 90 seconds"),
        finding = finding,
        unjudged = unjudged,
        mergeable = mergeable,
    )

    @Composable
    private fun review(cards: List<PairCard>, onAnswer: (Pair, PairAnswer) -> Unit = { _, _ -> }, undone: () -> Unit = {}) {
        EngramTheme {
            PairReview(
                cards = cards,
                onAnswer = { p, a -> onAnswer(p, a); "row-1" },
                onUndo = { undone(); true },
                onArtifact = {},
            )
        }
    }

    @Test fun whereAMergeWouldBeRefusedNoWriteOneIsDrawn() {
        compose.setContent { review(listOf(PairCard(pair(mergeable = false), 1))) }
        compose.onNodeWithText("Write one").assertDoesNotExist()
        compose.onNodeWithText("""Keep "Timeout 0"""").assertExists()
        compose.onNodeWithText("Discard both").assertExists()
        compose.onNodeWithText("Dismiss").assertExists()
    }

    @Test fun anUnjudgedPairDrawsTheMeasurementAndNotAFindingNobodyMade() {
        compose.setContent {
            review(listOf(PairCard(pair(unjudged = true, finding = "these two cover the same ground"), 1)))
        }
        compose.onNodeWithText("91% alike", substring = true).assertExists()
        compose.onNodeWithText("these two cover the same ground").assertDoesNotExist()
    }

    @Test fun aPairFromCoRetrievalDrawsNoPercentage() {
        compose.setContent { review(listOf(PairCard(pair(viaLink = true), 1))) }
        compose.onNodeWithText("recalled together", substring = true).assertExists()
        compose.onNodeWithText("91%", substring = true).assertDoesNotExist()
    }

    @Test fun theReviewAdvancesAfterAnAnswerAndTheUndoIsOnScreen() {
        val answers = mutableListOf<kotlin.Pair<Long, PairAnswer>>()
        compose.setContent {
            review(
                listOf(PairCard(pair(id = 1), 1), PairCard(pair(id = 2), 1)),
                onAnswer = { p, a -> answers += p.id to a },
            )
        }
        compose.onNodeWithText("1 of 2").assertExists()
        // Dismiss hides nothing, so it is the one answer with no confirmation.
        compose.onNodeWithText("Dismiss").performClick()
        compose.waitForIdle()

        assertEquals(listOf(1L to PairAnswer.Dismiss), answers)
        compose.onNodeWithText("2 of 2").assertExists()
        compose.onNodeWithText("Undo").assertExists()
    }

    @Test fun anAnswerThatHidesSomethingNamesWhatItHidesFirst() {
        val answers = mutableListOf<PairAnswer>()
        compose.setContent { review(listOf(PairCard(pair(), 1)), onAnswer = { _, a -> answers += a }) }
        compose.onNodeWithText("""Keep "Timeout 0"""").performClick()
        compose.waitForIdle()
        // Nothing has been enqueued yet: the confirmation names the cost.
        assertEquals(emptyList<PairAnswer>(), answers)
        compose.onNodeWithText("""Hides "Timeout 1".""").assertExists()
        compose.onNodeWithText("Do it").performClick()
        compose.waitForIdle()
        assertEquals(listOf(PairAnswer.KeepA), answers)
    }

    @Test fun anUndoTakesTheAnswerBackAndTheCardReturns() {
        var undone = 0
        compose.setContent { review(listOf(PairCard(pair(id = 1), 1), PairCard(pair(id = 2), 1)), undone = { undone++ }) }
        compose.onNodeWithText("Dismiss").performClick()
        compose.waitForIdle()
        compose.onNodeWithText("Undo").performClick()
        compose.waitForIdle()
        assertEquals(1, undone)
        compose.onNodeWithText("1 of 2").assertExists()
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
