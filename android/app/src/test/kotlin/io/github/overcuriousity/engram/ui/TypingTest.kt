package io.github.overcuriousity.engram.ui

import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * What the box asks while it is being typed into. The web's rule, and this is
 * the same rule: what was typed 120ms ago is a question, everything on the way
 * to it is not.
 */
class TypingTest {
    @Test fun onlyWhatTheTypingSettledOnIsAsked() = runTest {
        val typed = flow {
            emit("e"); emit("en"); emit("eng")
            delay(500)
            emit("engr"); emit("engram")
            delay(500)
        }
        assertEquals(listOf("eng", "engram"), typed.queries().toList())
    }

    /**
     * The trim is what makes this true, and it matters because the space after
     * a word is the most common keystroke there is: without it every word in a
     * sentence would be embedded twice on the way to the next one.
     */
    @Test fun aSpaceAfterAWordIsNotANewQuestion() = runTest {
        val typed = flow {
            emit("engram")
            delay(500)
            emit("engram ")
            delay(500)
        }
        assertEquals(listOf("engram"), typed.queries().toList())
    }

    /** An emptied box is asked as nothing, which is what puts the idle page back. */
    @Test fun emptyingTheBoxIsAskedAsNothing() = runTest {
        val typed = flow {
            emit("engram")
            delay(500)
            emit("")
            delay(500)
        }
        assertEquals(listOf("engram", ""), typed.queries().toList())
    }
}
