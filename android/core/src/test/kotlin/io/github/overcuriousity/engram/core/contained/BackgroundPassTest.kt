package io.github.overcuriousity.engram.core.contained

import android.os.PowerManager
import androidx.work.NetworkType
import org.junit.Assert.*
import org.junit.Test

class BackgroundPassTest {
    @Test fun thePassWantsAPhoneThatIsPluggedInIdleAndNotPayingForData() {
        val c = Passes.constraints()
        assertTrue(c.requiresCharging()); assertTrue(c.requiresDeviceIdle()); assertTrue(c.requiresBatteryNotLow())
        assertEquals(NetworkType.UNMETERED, c.requiredNetworkType)
    }

    @Test fun aWarmPhoneEndsThePassWhateverWaits() {
        assertTrue(Passes.shouldEnd(PowerManager.THERMAL_STATUS_MODERATE, 40))
        assertTrue(Passes.shouldEnd(PowerManager.THERMAL_STATUS_SEVERE, 40))
        assertFalse(Passes.shouldEnd(PowerManager.THERMAL_STATUS_LIGHT, 40))
        assertFalse(Passes.shouldEnd(PowerManager.THERMAL_STATUS_NONE, 1))
    }

    @Test fun aQueueRunDryOrOneThatCannotBeReadEndsItToo() {
        assertTrue(Passes.shouldEnd(PowerManager.THERMAL_STATUS_NONE, 0))
        assertTrue(Passes.shouldEnd(PowerManager.THERMAL_STATUS_NONE, null))
    }
}
