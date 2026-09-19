package io.github.overcuriousity.engram.core.contained

import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.pm.ServiceInfo
import androidx.core.app.NotificationCompat
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingWorkPolicy
import androidx.work.ForegroundInfo
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkInfo
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import androidx.work.workDataOf
import io.github.overcuriousity.engram.core.Engram
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map
import java.io.IOException

/** One model's download as the screens see it. */
data class Progress(val bytes: Long, val of: Long, val state: State, val error: String? = null) {
    enum class State { Idle, Waiting, Running, Done, Failed }
}

/**
 * A model download, as foreground work: hundreds of megabytes do not arrive
 * inside the ten minutes ordinary work is given, and the notification is what
 * lets them. What it does is the downloader's; this is only how it is kept
 * alive, shown, and tried again.
 */
class DownloadWorker(ctx: Context, params: WorkerParameters) : CoroutineWorker(ctx, params) {
    override suspend fun doWork(): Result {
        val model = ModelManifest.all.firstOrNull { it.file == inputData.getString(FILE) }
            ?: return Result.failure(workDataOf(ERROR to "not a model this build knows"))
        val downloader = Engram.get(applicationContext).downloader
            ?: return Result.failure(workDataOf(ERROR to "not in contained mode"))
        setForeground(info(model, 0))
        var told = 0L
        var shown = 0L
        return try {
            downloader.fetch(model) { bytes ->
                // Four times a second is as often as anybody can read a number.
                val now = System.currentTimeMillis()
                if (now - told >= 250) {
                    told = now
                    setProgressAsync(workDataOf(BYTES to bytes))
                }
                // The shade is allowed five a second for the whole app and
                // sheds the rest, as the first phone's log showed; a bar in
                // it moves well enough once a second.
                if (now - shown >= 1000) {
                    shown = now
                    notifications().notify(model.file.hashCode(), notification(model, bytes))
                }
            }
            Result.success()
        } catch (e: DownloadFailed) {
            Result.failure(workDataOf(ERROR to e.message))
        } catch (e: IOException) {
            // The network went. What arrived is kept, and the next run continues it.
            Result.retry()
        }
    }

    private fun notifications() = applicationContext.getSystemService(NotificationManager::class.java)

    private fun notification(model: Model, bytes: Long) =
        NotificationCompat.Builder(applicationContext, CHANNEL)
            .setSmallIcon(android.R.drawable.stat_sys_download)
            .setContentTitle(model.name)
            .setProgress(1000, (bytes * 1000 / model.bytes).toInt(), bytes == 0L)
            .setOngoing(true).setOnlyAlertOnce(true)
            .build()

    private fun info(model: Model, bytes: Long): ForegroundInfo {
        notifications().createNotificationChannel(NotificationChannel(CHANNEL, "Models", NotificationManager.IMPORTANCE_LOW))
        return ForegroundInfo(model.file.hashCode(), notification(model, bytes), ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
    }

    companion object {
        const val FILE = "file"
        const val BYTES = "bytes"
        const val ERROR = "error"
        private const val CHANNEL = "models"
    }
}

object Downloads {
    private fun name(model: Model) = "model-${model.file}"

    /** On an unmetered network unless the person has said this one may be paid for. */
    internal fun constraints(allowMetered: Boolean): Constraints =
        Constraints.Builder().setRequiredNetworkType(if (allowMetered) NetworkType.CONNECTED else NetworkType.UNMETERED).build()

    /** KEEP: a second press on a download that is running is not a second download. */
    fun start(context: Context, model: Model, allowMetered: Boolean) {
        WorkManager.getInstance(context).enqueueUniqueWork(
            name(model), ExistingWorkPolicy.KEEP,
            OneTimeWorkRequestBuilder<DownloadWorker>()
                .setInputData(workDataOf(DownloadWorker.FILE to model.file))
                .setConstraints(constraints(allowMetered))
                .build(),
        )
    }

    fun cancel(context: Context, model: Model) = WorkManager.getInstance(context).cancelUniqueWork(name(model))

    fun progress(context: Context, model: Model): Flow<Progress> =
        WorkManager.getInstance(context).getWorkInfosForUniqueWorkFlow(name(model)).map { progressOf(model, it.lastOrNull()) }

    /** What a work record means for the screen. Pure, so it is tested without a WorkManager. */
    internal fun progressOf(model: Model, info: WorkInfo?): Progress = when (info?.state) {
        null, WorkInfo.State.CANCELLED -> Progress(0, model.bytes, Progress.State.Idle)
        WorkInfo.State.ENQUEUED, WorkInfo.State.BLOCKED -> Progress(0, model.bytes, Progress.State.Waiting)
        WorkInfo.State.RUNNING -> Progress(info.progress.getLong(DownloadWorker.BYTES, 0), model.bytes, Progress.State.Running)
        WorkInfo.State.SUCCEEDED -> Progress(model.bytes, model.bytes, Progress.State.Done)
        WorkInfo.State.FAILED -> Progress(0, model.bytes, Progress.State.Failed, info.outputData.getString(DownloadWorker.ERROR))
    }
}
