package io.github.overcuriousity.engram.core.outbox

import org.junit.Assert.assertEquals
import org.junit.Test

class BackoffTest {
    @Test fun theScheduleIsTheSpecs() {
        assertEquals(30_000L, Backoff.delayMs(1))
        assertEquals(120_000L, Backoff.delayMs(2))
        assertEquals(600_000L, Backoff.delayMs(3))
        assertEquals(1_800_000L, Backoff.delayMs(4))
        assertEquals(3_600_000L, Backoff.delayMs(5))
        assertEquals(7_200_000L, Backoff.delayMs(6))
        assertEquals(7_200_000L, Backoff.delayMs(60))
    }
}
