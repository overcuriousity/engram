package io.github.overcuriousity.engram.ui

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import io.github.overcuriousity.engram.core.read.Chunk
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class DecisionsTest {
    @get:Rule val compose = createComposeRule()

    @Test fun whichAnswersAnArtifactAdmitsDependsOnWhetherItIsInResults() {
        assertEquals(listOf(ArtifactAnswer.Hide, ArtifactAnswer.Delete), answersFor("active", null))
        assertEquals(listOf(ArtifactAnswer.Reactivate, ArtifactAnswer.Delete), answersFor("deprecated", null))
        assertEquals(listOf(ArtifactAnswer.Reactivate, ArtifactAnswer.Delete), answersFor("active", "winner"))
    }

    @Test fun everyAnswerActsAtOnce() {
        var hidden = 0
        var deleted = 0
        compose.setContent {
            EngramTheme {
                Decisions(
                    chunk = Chunk(id = "a", text = "t", status = "active"),
                    decided = null,
                    onHide = { hidden++ }, onReactivate = {}, onDelete = { deleted++ },
                    onUndo = {}, onWinner = {},
                )
            }
        }
        compose.onNodeWithText("Still accurate").assertDoesNotExist()
        compose.onNodeWithText("Hide from results").performClick()
        assertEquals(1, hidden)
        compose.onNodeWithText("Delete").performClick()
        assertEquals(1, deleted)
    }

    @Test fun aDeletionIsShownWithItsUndo() {
        var undone: String? = null
        compose.setContent {
            EngramTheme {
                Decisions(
                    chunk = Chunk(id = "a", text = "t", status = "active"),
                    decided = Decision(ArtifactAnswer.Delete, row = "row-2"),
                    onHide = {}, onReactivate = {}, onDelete = {}, onUndo = { undone = it }, onWinner = {},
                )
            }
        }
        compose.onNodeWithText("✓ Deleted · in Trash for 30 days").assertIsDisplayed()
        compose.onNodeWithText("Undo").performClick()
        assertEquals("row-2", undone)
    }

    @Test fun aHiddenArtifactOffersTheWayBackAndNotHide() {
        compose.setContent {
            EngramTheme {
                Decisions(
                    chunk = Chunk(id = "a", text = "t", status = "active", supersededBy = "w"),
                    decided = null, onHide = {}, onReactivate = {}, onDelete = {}, onUndo = {}, onWinner = {},
                )
            }
        }
        compose.onNodeWithText("⚠ Hidden from results").assertExists()
        compose.onNodeWithText("Put it back").assertExists()
        compose.onNodeWithText("Hide from results").assertDoesNotExist()
        compose.onNodeWithText("Still accurate").assertDoesNotExist()
    }

    @Test fun aDecisionStandsOnTheScreenWithItsUndo() {
        var undone: String? = null
        compose.setContent {
            EngramTheme {
                Decisions(
                    chunk = Chunk(id = "a", text = "t", status = "active"),
                    decided = Decision(ArtifactAnswer.Hide, row = "row-1"),
                    onHide = {}, onReactivate = {}, onDelete = {}, onUndo = { undone = it }, onWinner = {},
                )
            }
        }
        compose.onNodeWithText("✓ Hidden from results · the artifact is kept").assertExists()
        compose.onNodeWithText("Hide from results").assertDoesNotExist()
        compose.onNodeWithText("Undo").performClick()
        assertEquals("row-1", undone)
    }
}
