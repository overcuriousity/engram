package io.github.overcuriousity.engram.ui

import org.junit.Assert.assertEquals
import org.junit.Test

class SizesTest {
    @Test fun megabytesAreWholeAndGigabytesHaveOneDecimal() {
        assertEquals("334 MB", sizeWords(333_590_944))
        assertEquals("190 MB", sizeWords(190_085_487))
        assertEquals("1.3 GB", sizeWords(1_280_835_840))
        assertEquals("2.7 GB", sizeWords(2_740_937_888))
        assertEquals("0 MB", sizeWords(0))
    }
}
