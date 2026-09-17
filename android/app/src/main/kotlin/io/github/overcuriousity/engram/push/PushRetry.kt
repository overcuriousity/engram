package io.github.overcuriousity.engram.push

import android.content.Context
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import io.github.overcuriousity.engram.core.Engram
import java.io.IOException
import java.util.concurrent.TimeUnit

/** Re-PUTs the stored registration until the server has it. The endpoint is never lost, only the delivery. */
class PushRetry(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result = try {
        Engram.get(applicationContext).push.resend()
        Result.success()
    } catch (e: IOException) {
        Result.retry()
    }

    companion object {
        fun schedule(context: Context) {
            WorkManager.getInstance(context).enqueueUniqueWork(
                "engram-push", ExistingWorkPolicy.REPLACE,
                OneTimeWorkRequestBuilder<PushRetry>()
                    .setConstraints(Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build())
                    .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 30, TimeUnit.SECONDS)
                    .build(),
            )
        }
    }
}
