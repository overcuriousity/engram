package io.github.overcuriousity.engram.core

import android.content.Context
import android.os.Build
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.MomentRow
import io.github.overcuriousity.engram.core.outbox.Drainer
import io.github.overcuriousity.engram.core.outbox.Outbox
import io.github.overcuriousity.engram.core.push.Push
import io.github.overcuriousity.engram.core.read.Reader
import io.github.overcuriousity.engram.core.read.ServerReader
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import java.io.File
import java.time.ZoneId

/** Everything the app and its receivers are allowed to touch, built once. */
class Engram private constructor(val app: Context, versionName: String) {
    val userAgent = userAgent(versionName, Build.MODEL)
    val deviceName = "engram for Android $versionName · ${Build.MODEL}"
    val store = ConnectionStore(File(app.filesDir, "connection"), KeystoreBox())
    val db = Db.open(app)
    val outbox = Outbox(db, File(app.filesDir, "outbox"))
    val push = Push(store, { transport() }, db)

    /** The last reminder a push carried. Room stays inside this module; the screens read this. */
    val latestMoment: Flow<MomentRow?> get() = db.momentsDao().latest()
    private val prefs = app.getSharedPreferences("engram", Context.MODE_PRIVATE)
    val counters = ViewCounters(prefs)
    val situation = Situation(AndroidSituationSource(app, counters), Stable.of(app))

    val refused = MutableStateFlow(false)
    val pinMismatch = MutableStateFlow<PinMismatch?>(null)
    val pushFailure = MutableStateFlow<String?>(null)

    var placeOn: Boolean
        get() = prefs.getBoolean("place", false)
        set(v) = prefs.edit().putBoolean("place", v).apply()

    internal fun transport(): Transport? = store.current.value?.let { Transport(it, userAgent) }

    private val server = ServerReader(
        transport = { transport() },
        dao = db.cacheDao(),
        onRefused = { refused.value = true },
        onPinMismatch = { pinMismatch.value = it },
    )

    /**
     * Where every screen gets what it shows. Typed as the interface on
     * purpose: this line is where a self-contained app chooses its on-device
     * reader instead, and nothing outside this class can tell the difference.
     */
    val reader: Reader = server

    /** Housekeeping the app runs once when it opens: what has not been current for a month goes. */
    suspend fun prune() = server.prune()
    internal fun drainer(): Drainer? =
        transport()?.let { Drainer(outbox, it, { ZoneId.systemDefault().id }, System::currentTimeMillis) }

    suspend fun vapid(): String = transport()?.vapid() ?: throw IllegalStateException("unpaired")

    suspend fun pair(uri: PairUri) {
        val c = Pairing.claim(uri, deviceName, userAgent)
        store.set(c)
        refused.value = false
        pinMismatch.value = null
        outbox.requeueRefused()
        // A registration that fails to reach the new server is retried by the
        // push worker; it must not fail the pairing.
        runCatching { push.resend() }
        Sync.kick(app)
    }

    suspend fun unpair() {
        Sync.cancel(app)
        push.onUnregistered()
        // What was read from this server leaves with it. The outbox does not:
        // what is owed stays owed, and a re-pair delivers it.
        server.forget()
        db.askedDao().clear()
        store.clear()
        refused.value = false
        pinMismatch.value = null
    }

    companion object {
        @Volatile private var instance: Engram? = null

        fun get(context: Context): Engram = instance ?: synchronized(this) {
            instance ?: run {
                val ctx = context.applicationContext
                val v = ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName ?: "0"
                Engram(ctx, v).also { instance = it }
            }
        }
    }
}
