package io.github.overcuriousity.engram.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class VersionsTest {
    @Test fun dottedNumbersCompare() {
        assertTrue(compareVersions("0.2.0", "0.1.0") > 0)
        assertEquals(0, compareVersions("0.1.0", "0.1.0"))
        assertEquals(0, compareVersions("0.1", "0.1.0"))
        assertTrue(compareVersions("x", "0.1.0") < 0)
    }
}
