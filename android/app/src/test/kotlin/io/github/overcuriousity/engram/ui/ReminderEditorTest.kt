package io.github.overcuriousity.engram.ui

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import io.github.overcuriousity.engram.core.read.Reminder
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * The reminder editor under an artifact's text, as the web's (#131) has it:
 * the same five names for the same five rules, anything else custom, and the
 * live failure of #133 — a one-off read as a daily Tuesday rule — put right
 * with one choice.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class ReminderEditorTest {
    @get:Rule val compose = createComposeRule()

    private val invented = "FREQ=DAILY;INTERVAL=1;BYDAY=TU"

    @Test fun theNamesAreTheWebsRulesAndAnyOtherRuleIsCustom() {
        assertEquals("once", repeatOf(null))
        assertEquals("weekly", repeatOf("FREQ=WEEKLY"))
        assertEquals(CUSTOM, repeatOf(invented))
        assertEquals(CUSTOM, repeatOf("FREQ=WEEKLY;BYDAY=MO"))
        assertNull(ruleOf("once", "ignored"))
        assertEquals("FREQ=MONTHLY", ruleOf("monthly", ""))
        assertEquals("FREQ=WEEKLY;BYDAY=MO,FR", ruleOf(CUSTOM, "  FREQ=WEEKLY;BYDAY=MO,FR "))
        assertNull("a blank custom rule is none, as on the web", ruleOf(CUSTOM, " "))
        assertEquals("+ reminder", reminderSummary(null))
        assertEquals("reminder", reminderSummary(Reminder("m", 1, "UTC", null)))
        assertEquals("reminder · yearly", reminderSummary(Reminder("m", 1, "UTC", "FREQ=YEARLY")))
    }

    @Test fun anInventedRuleIsShownAsCustomAndSavedAsAOneOffInItsOwnZone() {
        var saved: Triple<Long, String, String?>? = null
        compose.setContent {
            EngramTheme {
                ReminderEditor(Reminder("m", 1_900_000_000, "Europe/Berlin", invented), onSave = { a, z, r -> saved = Triple(a, z, r) }, onRemove = {})
            }
        }
        compose.onNodeWithText("reminder · custom").performClick()
        compose.onNodeWithText(invented).assertIsDisplayed()
        compose.onNodeWithText("once").performClick()
        compose.onNodeWithText("Save").performClick()
        assertEquals(Triple(1_900_000_000L, "Europe/Berlin", null), saved)
    }

    @Test fun aTypedRuleIsSavedAsTyped() {
        var rule: String? = "unset"
        compose.setContent {
            EngramTheme { ReminderEditor(Reminder("m", 5, "UTC", null), onSave = { _, _, r -> rule = r }, onRemove = {}) }
        }
        compose.onNodeWithText("reminder").performClick()
        compose.onNodeWithText(CUSTOM).performClick()
        compose.onNodeWithText("FREQ=WEEKLY;BYDAY=MO").performTextInput("FREQ=WEEKLY;BYDAY=MO,FR")
        compose.onNodeWithText("Save").performClick()
        assertEquals("FREQ=WEEKLY;BYDAY=MO,FR", rule)
    }

    @Test fun anOrdinaryEntryOffersToMakeOneAndHasNothingToRemove() {
        compose.setContent { EngramTheme { ReminderEditor(null, onSave = { _, _, _ -> }, onRemove = {}) } }
        compose.onNodeWithText("+ reminder").performClick()
        compose.onNodeWithText("when?").assertIsDisplayed()
        compose.onNodeWithText("Save").assertIsNotEnabled()
        compose.onNodeWithText("Remove").assertDoesNotExist()
    }

    @Test fun removeIsOfferedOnAReminderAndUndoWhileTheWriteIsOwed() {
        var removed = 0
        var undone = 0
        compose.setContent {
            EngramTheme {
                ReminderEditor(Reminder("m", 5, "UTC", "FREQ=DAILY"), onSave = { _, _, _ -> }, onRemove = { removed++ }, undo = { undone++ })
            }
        }
        compose.onNodeWithText("Undo").performClick()
        compose.onNodeWithText("reminder · daily").performClick()
        compose.onNodeWithText("Remove").performClick()
        assertEquals(1, removed)
        assertEquals(1, undone)
    }
}
