package io.github.overcuriousity.engram.ui

import org.junit.Assert.assertEquals
import org.junit.Test

class BackgroundWordsTest {
    @Test fun nothingWaitingIsSaidPlainly() = assertEquals("Background · nothing waiting", backgroundWords(0, passWanted = true))
    @Test fun workWithNowhereToGoSaysWhatIsMissing() = assertEquals("Background · 12 waiting · no endpoint", backgroundWords(12, passWanted = false))
    @Test fun workWithSomewhereToGoSaysWhatItWaitsFor() = assertEquals("Background · 3 waiting · charging and idle", backgroundWords(3, passWanted = true))
}
