package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.OutboxRow
import io.github.overcuriousity.engram.core.db.State
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The line under the box after a capture, and whether the queue earns its
 * place on the bar. Both read off the outbox rows and nothing else.
 */
class CaptureWordsTest {
    private fun row(state: State, attempts: Int = 0, error: String? = null) =
        OutboxRow(id = "o", kind = Kind.capture_text, payload = "{}", createdAt = 1, attempts = attempts, nextAt = 1, state = state, error = error)

    @Test fun aCaptureSaysWhatBecameOfIt() {
        assertEquals(CaptureWords("Keeping…"), captureWords(row(State.queued)))
        assertEquals(CaptureWords("Kept on the phone · sent when the server can be reached"), captureWords(row(State.queued, attempts = 2, error = "timeout")))
        assertEquals(CaptureWords("Kept"), captureWords(row(State.sent)))
        assertEquals(CaptureWords("Kept"), captureWords(null))
        val held = captureWords(row(State.held, error = "text is empty"))
        assertTrue(held.wrong); assertEquals("Not kept · text is empty · see Queue", held.text)
        assertTrue(captureWords(row(State.refused)).wrong)
    }

    @Test fun theQueueIsOnTheBarOnlyWhileItIsWorthAGlance() {
        assertFalse(queueWorthAGlance(emptyList()))
        assertFalse("delivered is history", queueWorthAGlance(listOf(row(State.sent))))
        assertTrue(queueWorthAGlance(listOf(row(State.sent), row(State.queued))))
        assertTrue("a refused row is waiting on a person", queueWorthAGlance(listOf(row(State.held))))
        assertTrue(queueWorthAGlance(listOf(row(State.refused))))
    }
}
