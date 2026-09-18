package io.github.overcuriousity.engram.ui

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onLast
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import io.github.overcuriousity.engram.core.read.Chunk
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * The answers an artifact admits, and the one that asks first. The rule is the
 * web pane's: verify and hide for an artifact in results, the way back for one
 * that is not, delete whatever its status — and delete never on one press.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class DecisionsTest {
    @get:Rule val compose = createComposeRule()

    @Test fun whichAnswersAnArtifactAdmitsDependsOnWhetherItIsInResults() {
        assertEquals(listOf(ArtifactAnswer.Verify, ArtifactAnswer.Hide, ArtifactAnswer.Delete), answersFor("active", null))
        assertEquals(listOf(ArtifactAnswer.Reactivate, ArtifactAnswer.Delete), answersFor("deprecated", null))
        assertEquals(listOf(ArtifactAnswer.Reactivate, ArtifactAnswer.Delete), answersFor("active", "winner"))
    }

    @Test fun deleteAsksFirstAndTheOthersDoNot() {
        var verified = 0
        var hidden = 0
        var deleted = 0
        compose.setContent {
            EngramTheme {
                Decisions(
                    chunk = Chunk(id = "a", text = "t", status = "active"),
                    decided = null,
                    onVerify = { verified++ }, onHide = { hidden++ }, onReactivate = {}, onDelete = { deleted++ },
                    onUndo = {}, onWinner = {},
                )
            }
        }
        compose.onNodeWithText("Still accurate").performClick()
        compose.onNodeWithText("Hide from results").performClick()
        assertEquals(1, verified); assertEquals(1, hidden)

        compose.onNodeWithText("Delete").performClick()
        assertEquals("a press is a question, not a deletion", 0, deleted)
        compose.onNodeWithText("Delete this artifact for good?").assertIsDisplayed()
        compose.onNodeWithText("Keep it").performClick()
        assertEquals(0, deleted)

        compose.onAllNodesWithText("Delete").onLast().performClick()
        compose.onNodeWithText("Delete this artifact for good?").assertIsDisplayed()
        // The dialog's own Delete is the last node of that name: the one on
        // the row is behind it.
        compose.onAllNodesWithText("Delete").onLast().performClick()
        assertEquals(1, deleted)
    }

    @Test fun aHiddenArtifactOffersTheWayBackAndNotHide() {
        compose.setContent {
            EngramTheme {
                Decisions(
                    chunk = Chunk(id = "a", text = "t", status = "active", supersededBy = "w"),
                    decided = null, onVerify = {}, onHide = {}, onReactivate = {}, onDelete = {}, onUndo = {}, onWinner = {},
                )
            }
        }
        compose.onNodeWithText("Hidden from results").assertExists()
        compose.onNodeWithText("Put it back").assertExists()
        compose.onNodeWithText("Hide from results").assertDoesNotExist()
        compose.onNodeWithText("Still accurate").assertDoesNotExist()
    }

    /** A decision made here is shown as made, with its undo, before the server has heard of it. */
    @Test fun aDecisionStandsOnTheScreenWithItsUndo() {
        var undone: String? = null
        compose.setContent {
            EngramTheme {
                Decisions(
                    chunk = Chunk(id = "a", text = "t", status = "active"),
                    decided = Decision(ArtifactAnswer.Hide, row = "row-1"),
                    onVerify = {}, onHide = {}, onReactivate = {}, onDelete = {}, onUndo = { undone = it }, onWinner = {},
                )
            }
        }
        compose.onNodeWithText("Hidden from results · the artifact is kept").assertExists()
        compose.onNodeWithText("Hide from results").assertDoesNotExist()
        compose.onNodeWithText("Undo").performClick()
        assertEquals("row-1", undone)
    }
}
