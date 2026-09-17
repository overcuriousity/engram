package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.OutboxRow
import io.github.overcuriousity.engram.core.db.State
import org.junit.Assert.assertEquals
import org.junit.Test

class RowWordsTest {
    private fun row(state: State, status: Int? = null, error: String? = null, nextAt: Long = 0) =
        OutboxRow("id", Kind.capture_text, "{}", 0, 0, nextAt, state, status, null, error)

    @Test fun eachStateHasItsWords() {
        assertEquals("waiting", rowWords(row(State.queued), now = 10))
        assertEquals("waiting · next try in 2 min", rowWords(row(State.queued, nextAt = 130_000), now = 10_000))
        assertEquals("stored", rowWords(row(State.sent, 201), now = 0))
        assertEquals("already held", rowWords(row(State.sent, 200), now = 0))
        assertEquals("stored · still being read", rowWords(row(State.sent, 202), now = 0))
        assertEquals("held for review · that body is not valid UTF-8 text", rowWords(row(State.held, 400, "that body is not valid UTF-8 text"), now = 0))
        assertEquals("refused · scan a new code", rowWords(row(State.refused), now = 0))
    }
}
