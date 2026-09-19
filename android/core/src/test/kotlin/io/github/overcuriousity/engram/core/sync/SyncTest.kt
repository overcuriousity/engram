package io.github.overcuriousity.engram.core.sync

import androidx.work.NetworkType
import org.junit.Assert.assertEquals
import org.junit.Test

class SyncTest {
    @Test fun aServerIsOwedOverANetwork() =
        assertEquals(NetworkType.CONNECTED, Sync.constraints(loopback = false).requiredNetworkType)

    @Test fun theCoreInThisProcessIsOwedInAeroplaneModeToo() =
        assertEquals(NetworkType.NOT_REQUIRED, Sync.constraints(loopback = true).requiredNetworkType)
}
