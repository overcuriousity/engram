package io.github.overcuriousity.engram.core.contained

import androidx.work.NetworkType
import org.junit.Assert.assertEquals
import org.junit.Test

class DownloadsTest {
    private val model = ModelManifest.required.single()

    @Test fun aDownloadWaitsForANetworkNobodyPaysForByTheMegabyte() =
        assertEquals(NetworkType.UNMETERED, Downloads.constraints(allowMetered = false).requiredNetworkType)

    @Test fun unlessThePersonSaidThisOneMay() =
        assertEquals(NetworkType.CONNECTED, Downloads.constraints(allowMetered = true).requiredNetworkType)

    @Test fun workNobodyStartedIsIdleAtTheModelsSize() =
        assertEquals(Progress(0, model.bytes, Progress.State.Idle), Downloads.progressOf(model, null))
}
