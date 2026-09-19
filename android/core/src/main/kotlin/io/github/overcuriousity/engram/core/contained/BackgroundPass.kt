package io.github.overcuriousity.engram.core.contained

import android.content.Context
import android.os.PowerManager
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import io.github.overcuriousity.engram.core.Engram
import kotlinx.coroutines.delay
import java.util.concurrent.TimeUnit

/**
 * The one pass in which a contained phone does its model work: reading
 * captures, judging pairs, the sweeps that write. Everything a person waits
 * for — cutting, embedding, indexing — has already happened by then, without
 * it. This opens the core's gate while the phone is charging, idle and cool,
 * and shuts it again whatever ends the pass.
 */
class BackgroundPass(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result {
        val engram = Engram.get(applicationContext)
        if (!engram.passWanted || !engram.ready()) return Result.success()
        val power = applicationContext.getSystemService(PowerManager::class.java)
        try {
            // Shut where there is nothing to do the work; then there is no pass.
            if (!Core.background(true)) return Result.success()
            while (!Passes.shouldEnd(power.currentThermalStatus, engram.waitingGeneration())) delay(CHECK_MS)
        } finally {
            // Also what a lapsed constraint reaches: WorkManager cancels the
            // coroutine when the charger is pulled, and the gate must not stay
            // open behind it.
            Core.background(false)
        }
        return Result.success()
    }

    private companion object { const val CHECK_MS = 30_000L }
}

object Passes {
    private const val NAME = "engram-background-pass"

    /** Charging, idle, battery not low, and a network nobody pays for by the megabyte: the work goes to an endpoint. */
    internal fun constraints(): Constraints = Constraints.Builder()
        .setRequiresCharging(true)
        .setRequiresDeviceIdle(true)
        .setRequiresBatteryNotLow(true)
        .setRequiredNetworkType(NetworkType.UNMETERED)
        .build()

    /**
     * Whether the pass is over. A phone that has grown warm ends it whatever
     * waits — the check between jobs the design asks for — and so does a queue
     * that has run dry. An unreadable count is treated as dry: a pass that
     * cannot see its work does not hold the gate open on a guess.
     */
    internal fun shouldEnd(thermal: Int, waiting: Int?): Boolean =
        thermal >= PowerManager.THERMAL_STATUS_MODERATE || waiting == null || waiting <= 0

    fun schedule(context: Context) {
        WorkManager.getInstance(context).enqueueUniquePeriodicWork(
            NAME, ExistingPeriodicWorkPolicy.KEEP,
            PeriodicWorkRequestBuilder<BackgroundPass>(6, TimeUnit.HOURS).setConstraints(constraints()).build(),
        )
    }

    fun cancel(context: Context) = WorkManager.getInstance(context).cancelUniqueWork(NAME)
}
