package io.github.overcuriousity.engram.ui

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import io.github.overcuriousity.engram.core.PairUri
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** A pairing code is claimed at once only where the person's own hand brought it. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class PairArrivalTest {
    @get:Rule val compose = createComposeRule()
    private val code = "engram://pair?o=https://evil.example&c=x&v=9.0.0"
    private val uri = PairUri.parse(code)!!

    @Test fun aCodeThatCameByLinkWaitsForAPress() {
        assertEquals(Arrival.Offer(uri), arrival(code, byLink = true, knownOrigin = null))
    }

    @Test fun aScannedCodeOnAnUnpairedPhoneIsClaimed() {
        assertEquals(Arrival.Claim(uri), arrival(code, byLink = false, knownOrigin = null))
    }

    @Test fun aScannedCodeForAnotherServerThanTheOneHeldWaitsForAPress() {
        assertEquals(Arrival.Offer(uri), arrival(code, byLink = false, knownOrigin = "https://mine.example"))
        assertEquals(Arrival.Claim(uri), arrival(code, byLink = false, knownOrigin = "https://evil.example"))
    }

    @Test fun whatIsNotACodeIsRefused() {
        assertEquals(Arrival.NotACode, arrival("https://evil.example", byLink = true, knownOrigin = null))
    }

    @Test fun theOfferNamesTheServerAndPairsOnlyOnThePress() {
        val pressed = mutableListOf<String>()
        compose.setContent {
            EngramTheme { PairOffer(uri, "https://mine.example", { pressed += "pair" }, { pressed += "cancel" }) }
        }
        compose.onNodeWithText("https://evil.example").assertExists()
        compose.onNodeWithText("Replaces https://mine.example").assertExists()
        assertTrue(pressed.isEmpty())
        compose.onNodeWithText("Pair").performClick()
        assertEquals(listOf("pair"), pressed)
    }
}
