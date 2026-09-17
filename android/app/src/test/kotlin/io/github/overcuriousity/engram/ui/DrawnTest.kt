package io.github.overcuriousity.engram.ui

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import io.github.overcuriousity.engram.core.read.Hit
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
}
