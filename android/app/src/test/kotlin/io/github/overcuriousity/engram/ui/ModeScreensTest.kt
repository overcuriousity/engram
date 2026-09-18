package io.github.overcuriousity.engram.ui

import androidx.compose.material3.Text
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import io.github.overcuriousity.engram.core.Mode
import io.github.overcuriousity.engram.core.contained.ModelManifest
import io.github.overcuriousity.engram.core.contained.Progress
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** The screens of choosing, fetching and switching, drawn from what they are handed. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class ModeScreensTest {
    @get:Rule val compose = createComposeRule()
    private val embed = ModelManifest.required.single()
    private fun idle() = Progress(0, embed.bytes, Progress.State.Idle)

    @Test fun theChooserOffersBothAndSaysWhatThePhoneCosts() {
        val pressed = mutableListOf<String>()
        compose.setContent { EngramTheme { ModeChooser(true, sizeWords(embed.bytes), { pressed += "phone" }, { pressed += "server" }) } }
        compose.onNodeWithText("334 MB", substring = true).assertExists()
        compose.onNodeWithText("On this phone").performClick()
        compose.onNodeWithText("With a server").performClick()
        assertEquals(listOf("phone", "server"), pressed)
    }

    @Test fun whereTheCoreIsAbsentOnlyAServerIsOffered() {
        compose.setContent { EngramTheme { ModeChooser(false, "334 MB", {}, {}) } }
        compose.onNodeWithText("On this phone").assertDoesNotExist()
        compose.onNodeWithText("With a server").assertExists()
    }

    @Test fun aModelRowSaysWhatItIsAndWhose() {
        var downloads = 0; var terms = ""
        compose.setContent { EngramTheme { ModelRow(embed, idle(), installed = false, { downloads++ }, {}, {}, { terms = it }) } }
        compose.onNodeWithText("EmbeddingGemma 300M").assertExists()
        compose.onNodeWithText("334 MB · Gemma Terms of Use").performClick()
        compose.onNodeWithText("Download").performClick()
        assertEquals(1, downloads)
        assertEquals("https://ai.google.dev/gemma/terms", terms)
    }

    @Test fun aRunningDownloadShowsHowFarAndCanBeCancelled() {
        var cancelled = 0
        compose.setContent { EngramTheme { ModelRow(embed, Progress(167_000_000, embed.bytes, Progress.State.Running), false, {}, { cancelled++ }, {}) } }
        compose.onNodeWithText("167 MB of 334 MB").assertExists()
        compose.onNodeWithText("Download").assertDoesNotExist()
        compose.onNodeWithText("Cancel").performClick()
        assertEquals(1, cancelled)
    }

    @Test fun aWaitingDownloadSaysWhatItWaitsFor() {
        compose.setContent { EngramTheme { ModelRow(embed, Progress(0, embed.bytes, Progress.State.Waiting), false, {}, {}, {}) } }
        compose.onNodeWithText("Waiting for Wi-Fi").assertExists()
        compose.onNodeWithText("Cancel").assertExists()
    }

    @Test fun anInstalledModelCanBeRemoved() {
        var removed = 0
        compose.setContent { EngramTheme { ModelRow(embed, Progress(embed.bytes, embed.bytes, Progress.State.Done), true, {}, {}, { removed++ }) } }
        compose.onNodeWithText("Installed").assertExists()
        compose.onNodeWithText("Remove").performClick()
        assertEquals(1, removed)
    }

    @Test fun aFailedDownloadSaysWhyAndCanBeRetried() {
        var again = 0
        compose.setContent { EngramTheme { ModelRow(embed, Progress(0, embed.bytes, Progress.State.Failed, "EmbeddingGemma 300M did not verify"), false, { again++ }, {}, {}) } }
        compose.onNodeWithText("failed · EmbeddingGemma 300M did not verify").assertExists()
        compose.onNodeWithText("Retry").performClick()
        assertEquals(1, again)
    }

    @Test fun theMeteredDialogNamesTheSizeAndBothWaysOn() {
        val pressed = mutableListOf<String>()
        compose.setContent { EngramTheme { MeteredDialog("334 MB", { pressed += "anyway" }, { pressed += "wait" }, {}) } }
        compose.onNodeWithText("Metered network · 334 MB").assertExists()
        compose.onNodeWithText("Wait for Wi-Fi").performClick()
        compose.onNodeWithText("Download anyway").performClick()
        assertEquals(listOf("wait", "anyway"), pressed)
    }

    @Test fun switchingSaysOnceThatNothingIsCopied() {
        var switched = 0
        compose.setContent { EngramTheme { SwitchDialog(Mode.server, { switched++ }, {}) } }
        compose.onNodeWithText("Switch to a server?").assertExists()
        compose.onNodeWithText("Separate bases · nothing is copied").assertExists()
        compose.onNodeWithText("Switch").performClick()
        assertEquals(1, switched)
    }

    @Test fun switchingBackIsAskedInItsOwnWords() {
        compose.setContent { EngramTheme { SwitchDialog(Mode.contained, {}, {}) } }
        compose.onNodeWithText("Switch to this phone?").assertExists()
        compose.onNodeWithText("Stay").assertExists()
    }

    @Test fun theAskOfferHasThreeWaysOn() {
        val pressed = mutableListOf<String>()
        compose.setContent { EngramTheme { AskOfferPane({ Text("the models") }, { pressed += "endpoint" }, { pressed += "off" }) } }
        compose.onNodeWithText("Ask · no model yet").assertExists()
        compose.onNodeWithText("the models").assertExists()
        compose.onNodeWithText("Use an endpoint").performClick()
        compose.onNodeWithText("Leave Ask off").performClick()
        assertEquals(listOf("endpoint", "off"), pressed)
    }

    @Test fun askOffSaysSoAndPointsAtSettings() {
        var opened = 0
        compose.setContent { EngramTheme { AskOffPane { opened++ } } }
        compose.onNodeWithText("Ask · off").assertExists()
        compose.onNodeWithText("Settings").performClick()
        assertEquals(1, opened)
    }
}
