package io.github.overcuriousity.engram.core.sync

import android.content.Context
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.outbox.Drainer
import java.util.concurrent.TimeUnit

/** Drains the outbox. Unique, so two never run at once; rescheduled at the nearest rung after each pass. */
class SyncWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result {
        val engram = Engram.get(applicationContext)
        val drainer = engram.drainer() ?: return Result.success() // unpaired: nothing owed to anyone
        engram.outbox.sweepSent(olderThanMs = 7L * 24 * 3600 * 1000)
        return when (val out = drainer.drainOnce()) {
            Drainer.Outcome.Done -> Result.success()
            is Drainer.Outcome.Later -> { Sync.scheduleAt(applicationContext, out.nextAt); Result.success() }
            Drainer.Outcome.Refused -> { engram.refused.value = true; Result.success() }
            is Drainer.Outcome.Pinned -> { engram.pinMismatch.value = out.e; Result.success() }
        }
    }
}

object Sync {
    private const val NAME = "engram-sync"
    private val online = Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build()

    /** Something new is owed: run as soon as there is a network. */
    fun kick(context: Context) {
        WorkManager.getInstance(context).enqueueUniqueWork(
            NAME, ExistingWorkPolicy.KEEP,
            OneTimeWorkRequestBuilder<SyncWorker>().setConstraints(online).build(),
        )
    }

    fun scheduleAt(context: Context, atMs: Long) {
        val delay = (atMs - System.currentTimeMillis()).coerceAtLeast(0)
        WorkManager.getInstance(context).enqueueUniqueWork(
            NAME, ExistingWorkPolicy.REPLACE,
            OneTimeWorkRequestBuilder<SyncWorker>().setConstraints(online)
                .setInitialDelay(delay, TimeUnit.MILLISECONDS).build(),
        )
    }

    fun cancel(context: Context) = WorkManager.getInstance(context).cancelUniqueWork(NAME)
}
