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
import io.github.overcuriousity.engram.core.reminders.LocalReminders
import java.util.concurrent.TimeUnit

/** Drains the outbox. Unique, so two never run at once; rescheduled at the nearest rung after each pass. */
class SyncWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result {
        val engram = Engram.get(applicationContext)
        // In contained mode this may be the first thing to need the core: a
        // share is an outbox row and a kick, with no activity in front.
        if (!engram.ready()) return Result.success()
        val drainer = engram.drainer() ?: return Result.success() // unpaired: nothing owed to anyone
        engram.outbox.sweepSent(olderThanMs = 7L * 24 * 3600 * 1000)
        val out = drainer.drainOnce()
        // A capture, a Done, a snooze, a new date: any of what was just
        // delivered can move what is due. Nothing pushes to a phone that is
        // its own engram, so it looks.
        LocalReminders.refresh(engram)
        return when (out) {
            Drainer.Outcome.Done -> Result.success()
            is Drainer.Outcome.Later -> { Sync.scheduleAt(applicationContext, out.nextAt); Result.success() }
            Drainer.Outcome.Refused -> { engram.refused.value = true; Result.success() }
            is Drainer.Outcome.Pinned -> { engram.pinMismatch.value = out.e; Result.success() }
        }
    }
}

object Sync {
    private const val NAME = "engram-sync"

    /** A server is reached over a network. The core in this process is not, and must not wait for one. */
    internal fun constraints(loopback: Boolean): Constraints =
        Constraints.Builder().setRequiredNetworkType(if (loopback) NetworkType.NOT_REQUIRED else NetworkType.CONNECTED).build()

    private fun constraints(context: Context) = constraints(Engram.get(context).loopback)

    /**
     * Something new is owed: run as soon as there is a network.
     *
     * REPLACE, not KEEP. `scheduleAt` parks a run under this same unique name
     * with a backoff delay that reaches hours, and KEEP would let that parked
     * run swallow every kick after it: a share, a Done from a notification, a
     * re-pair, "Deliver now" — all silently dropped while the capture sat in
     * the outbox with a working network. A kick means now, so it displaces
     * whatever was waiting; `drainOnce` is restartable, so replacing a pass
     * already running costs nothing but the pass.
     */
    fun kick(context: Context) {
        WorkManager.getInstance(context).enqueueUniqueWork(
            NAME, ExistingWorkPolicy.REPLACE,
            OneTimeWorkRequestBuilder<SyncWorker>().setConstraints(constraints(context)).build(),
        )
    }

    fun scheduleAt(context: Context, atMs: Long) {
        val delay = (atMs - System.currentTimeMillis()).coerceAtLeast(0)
        WorkManager.getInstance(context).enqueueUniqueWork(
            NAME, ExistingWorkPolicy.REPLACE,
            OneTimeWorkRequestBuilder<SyncWorker>().setConstraints(constraints(context))
                .setInitialDelay(delay, TimeUnit.MILLISECONDS).build(),
        )
    }

    fun cancel(context: Context) = WorkManager.getInstance(context).cancelUniqueWork(NAME)
}
