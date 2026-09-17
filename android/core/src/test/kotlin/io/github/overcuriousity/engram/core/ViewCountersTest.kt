package io.github.overcuriousity.engram.core

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class ViewCountersTest {
    private val prefs = ApplicationProvider.getApplicationContext<Context>()
        .getSharedPreferences("counters-test", Context.MODE_PRIVATE)
    private val counters = ViewCounters(prefs)

    @Test fun theCountIsWhatWasMarked() {
        assertEquals(0, counters.viewsToday())
        counters.mark()
        // The browser's first bundle of a day says 1, and so does this one.
        assertEquals(1, counters.viewsToday())
        counters.mark()
        assertEquals(2, counters.viewsToday())
    }

    @Test fun readingTheCountDoesNotRaiseIt() {
        counters.mark()
        repeat(5) { counters.viewsToday() }
        assertEquals(1, counters.viewsToday())
    }

    @Test fun aCountFromAnotherDayIsNotTodays() {
        counters.mark()
        prefs.edit().putString("views_day", "1999-12-31").apply()
        assertEquals(0, counters.viewsToday())
    }
}
