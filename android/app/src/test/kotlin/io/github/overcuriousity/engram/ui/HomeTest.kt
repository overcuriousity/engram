package io.github.overcuriousity.engram.ui

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * One box, and the verbs under it. The named failure is a second box coming
 * back — a Capture place in the bar, or a screen of its own — which is a
 * question put to the person before they are allowed to type.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class HomeTest {
    @get:Rule val compose = createComposeRule()

    @Test fun theBarHoldsNoPlaceToCaptureInBecauseTheBoxIsThePlace() {
        assertEquals(listOf("Search", "Today", "Library"), BAR.map { it.label })
        assertFalse(BAR.any { it.route == "compose" })
    }

    @Test fun theOneBoxSearchesAsksAndKeeps() {
        var text by mutableStateOf("")
        var asked = 0
        var captured = 0
        compose.setContent {
            EngramTheme {
                HomeBox(
                    text = text, onText = { text = it },
                    onAsk = { asked++ }, onCapture = { captured++ },
                )
            }
        }

        // Empty, the verbs are there and neither of them can be pressed: there
        // is nothing to ask and nothing to keep.
        compose.onNodeWithText("Ask, search, or paste to keep…").assertExists()
        compose.onNodeWithText("Ask").assertIsNotEnabled()
        compose.onNodeWithText("Capture").assertIsNotEnabled()
        // The doors the phone has that the browser does not, and the one it
        // shares with it: a microphone that is held, not a Record verb.
        compose.onNodeWithText("Attach").assertExists()
        compose.onNodeWithText("Photo").assertExists()
        compose.onNodeWithText("Record").assertDoesNotExist()
        compose.onNodeWithContentDescription("Hold to dictate").assertExists()

        compose.onNodeWithText("Ask, search, or paste to keep…").performTextInput("qdrant filters")
        compose.onNodeWithText("Ask").assertIsEnabled()
        compose.onNodeWithText("Capture").assertIsEnabled()
        compose.onNodeWithText("Capture").performClick()
        compose.onNodeWithText("Ask").performClick()
        assertEquals(1, captured)
        assertEquals(1, asked)
    }

    /** Title and note are a fold, so the surface stays one box until asked. */
    @Test fun titleAndNoteAreOutOfTheWayUntilTheyAreWanted() {
        compose.setContent { EngramTheme { HomeBox(text = "x", onText = {}) } }
        compose.onNodeWithText("Title").assertDoesNotExist()
        compose.onNodeWithText("Title · note").performClick()
        compose.onNodeWithText("Title").assertExists()
        compose.onNodeWithText("Note").assertExists()
    }

    /** Where the server has no speech model there is no button to hold — the web's rule. */
    @Test fun noSpeechModelNoMicrophone() {
        compose.setContent { EngramTheme { HomeBox(text = "", onText = {}, mic = null) } }
        compose.onNodeWithContentDescription("Hold to dictate").assertDoesNotExist()
    }

    /** What the microphone says stands under the box, and only while it has something to say. */
    @Test fun theMicrophoneSaysWhereItStands() {
        compose.setContent { EngramTheme { HomeBox(text = "", onText = {}, mic = MicState(busy = true, said = "Transcribing…")) } }
        compose.onNodeWithText("Transcribing…").assertExists()
    }
}
