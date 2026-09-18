package io.github.overcuriousity.engram.core

import android.content.Context
import android.net.ConnectivityManager
import android.os.Build
import io.github.overcuriousity.engram.core.ask.Ask
import io.github.overcuriousity.engram.core.contained.Contained
import io.github.overcuriousity.engram.core.contained.Core
import io.github.overcuriousity.engram.core.contained.CoreState
import io.github.overcuriousity.engram.core.contained.Downloader
import io.github.overcuriousity.engram.core.contained.Endpoint
import io.github.overcuriousity.engram.core.contained.Model
import io.github.overcuriousity.engram.core.contained.ModelManifest
import io.github.overcuriousity.engram.core.contained.Setup
import io.github.overcuriousity.engram.core.contained.Started
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.db.MomentRow
import io.github.overcuriousity.engram.core.outbox.Drainer
import io.github.overcuriousity.engram.core.outbox.Outbox
import io.github.overcuriousity.engram.core.push.Push
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.Reader
import io.github.overcuriousity.engram.core.read.ServerReader
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import okhttp3.OkHttpClient
import java.io.File
import java.time.ZoneId
import java.util.concurrent.TimeUnit

/** Everything the app and its receivers are allowed to touch, built once. */
class Engram internal constructor(
    val app: Context,
    versionName: String,
    box: SecretBox = KeystoreBox(),
    boot: ((String, Setup) -> Started)? = null,
    halt: (() -> Unit)? = null,
) {
    val userAgent = userAgent(versionName, Build.MODEL)
    val deviceName = "engram for Android $versionName · ${Build.MODEL}"
    private val prefs = app.getSharedPreferences("engram", Context.MODE_PRIVATE)
    val store = ConnectionStore(File(app.filesDir, "connection"), box)

    /**
     * Where this app's engram lives, read once: everything below is built for
     * one mode, and changing it is a new process. Nothing chosen is `server`,
     * so a phone that was paired before there was a choice carries on as it was.
     *
     * This and [transport] are the only places that know. Above here there is
     * a connection or there is not.
     */
    val modes = ModeStore(prefs)
    val mode: Mode = modes.chosen ?: Mode.server
    private val state = ModeState.of(mode, app.filesDir)
    private val contained: Contained? =
        if (mode == Mode.contained) Contained(
            state.core!!, ::setup, deviceName,
            // The verifier wants its Context before the core's first HTTPS call.
            boot ?: { dir, setup -> Core.init(app); Core.start(dir, setup) },
            halt ?: Core::shutdown,
        ) else null

    /** What the core is started with: the models that are here, and ask as the person set it. */
    private fun setup(): Setup {
        val m = state.models()
        return when (modes.ask) {
            AskVia.device -> Setup(m)
            AskVia.endpoint -> Setup(m.copy(ask = null), store.askEndpoint)
            AskVia.off -> Setup(m.copy(ask = null))
        }
    }

    /** Fetches models into contained mode's directory. Null in server mode, which has none. */
    val downloader: Downloader? = state.models?.let { Downloader(it, OkHttpClient.Builder().readTimeout(60, TimeUnit.SECONDS).build()) }
    fun installed(model: Model): Boolean = downloader?.installed(model) != null

    var askEndpoint: Endpoint?
        get() = store.askEndpoint
        set(v) { store.askEndpoint = v }

    /** A new core over what is on the phone now: after a model arrives or goes, or ask is set differently. */
    suspend fun restartCore(): Boolean = contained?.restart() != null

    val db = Db.open(app, state.dbName)
    val outbox = Outbox(db, state.outbox)
    val push = Push(store, { transport() }, db)

    /** What the app is talking to, if anything. A pairing in server mode; the running core in contained. */
    val connection: StateFlow<Connection?> = contained?.connected ?: store.current

    /** The core's own story, for the screen that waits on it. Null in server mode. */
    val core: StateFlow<CoreState>? = contained?.state

    /** True where the source is this process: nothing it is owed waits for a network. */
    val loopback: Boolean get() = contained != null

    /**
     * Whether there is something to talk to, starting the core if that is what
     * it takes. The worker asks before draining and the first screen asks
     * before drawing; whoever is first pays for the start.
     */
    suspend fun ready(): Boolean = (contained?.ensure() ?: store.current.value) != null

    internal fun close() = db.close()

    /** What contained mode cannot open without, and does not have. Empty in server mode. */
    fun requiredMissing(): List<Model> = if (contained == null) emptyList() else ModelManifest.required.filterNot(::installed)

    /** Ask is set to the phone and the phone has nothing to answer with: the moment for the offer. */
    val askWantsAModel: Boolean get() = contained != null && modes.ask == AskVia.device && state.models().ask == null

    val metered: Boolean get() = app.getSystemService(ConnectivityManager::class.java)?.isActiveNetworkMetered ?: false

    /** Model work has somewhere to go: contained, ask set to an endpoint, and one written down. */
    val passWanted: Boolean get() = contained != null && modes.ask == AskVia.endpoint && store.askEndpoint != null

    /** How much model work waits in the core's queue. Null where it cannot be asked. */
    suspend fun waitingGeneration(): Int? = runCatching {
        transport()?.get("/api/v1/status")?.takeIf { it.status == 200 }?.let { Decode.status(it.body).waitingGeneration }
    }.getOrNull()

    /** The end of this instance: the core stopped, the database closed. Nothing may use it afterwards. */
    suspend fun shutdown() { contained?.stop(); db.close() }

    /** The last reminder a push carried. Room stays inside this module; the screens read this. */
    val latestMoment: Flow<MomentRow?> get() = db.momentsDao().latest()
    val counters = ViewCounters(prefs)
    val situation = Situation(AndroidSituationSource(app, counters), Stable.of(app))

    /**
     * Fire-and-forget telling that must outlive the screen that started it:
     * a dwell reported as the pane closes, where the composable's own scope
     * is already cancelled and the launch would never run. Process-lifetime
     * on purpose, and only for what nothing on screen waits for.
     */
    val telling = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    val refused = MutableStateFlow(false)
    val pinMismatch = MutableStateFlow<PinMismatch?>(null)
    val pushFailure = MutableStateFlow<String?>(null)

    var placeOn: Boolean
        get() = prefs.getBoolean("place", false)
        set(v) = prefs.edit().putBoolean("place", v).apply()

    /** The theme chosen on this phone: `system`, `light` or `dark`. A word, so the screens own the enum. */
    val theme = MutableStateFlow(prefs.getString("theme", "system") ?: "system")
    fun setTheme(word: String) {
        prefs.edit().putString("theme", word).apply()
        theme.value = word
    }

    internal fun transport(): Transport? = when (contained) {
        null -> store.current.value?.let { Transport(it, userAgent) }
        else -> contained.connected.value?.let { Transport(it, userAgent, source = "contained") }
    }

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

    /** The one thing that is not a read: a question put to the server, answered as a stream. */
    val ask = Ask({ transport() }, db.askedDao(), { refused.value = true }, { pinMismatch.value = it })

    /** Housekeeping the app runs once when it opens: what has not been current for a month goes. */
    suspend fun prune() = server.prune()
    internal fun drainer(): Drainer? =
        transport()?.let { Drainer(outbox, it, { ZoneId.systemDefault().id }, System::currentTimeMillis) }

    suspend fun vapid(): String = transport()?.vapid() ?: throw IllegalStateException("unpaired")

    /** A captured photo's preview, as bytes. Null where there is none, or the server cannot be reached. */
    suspend fun picture(corpusId: String): ByteArray? =
        runCatching { transport()?.bytes("/api/v1/corpora/$corpusId/image") }.getOrNull()

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
        private val _current = MutableStateFlow<Engram?>(null)

        /** The instance in use. It changes when the mode does, and whatever draws from one re-draws from the next. */
        val current: StateFlow<Engram?> get() = _current

        private fun build(ctx: Context): Engram {
            val v = ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName ?: "0"
            return Engram(ctx, v)
        }

        fun get(context: Context): Engram = instance ?: synchronized(this) {
            instance ?: build(context.applicationContext).also { instance = it; _current.value = it }
        }

        /**
         * Store the mode and become an engram built for it. The two modes share
         * nothing, so there is nothing to carry over: the old instance is shut
         * and a new one reads the new mode, as a fresh process would.
         */
        suspend fun switch(context: Context, mode: Mode): Engram {
            val old = get(context)
            if (old.mode == mode && old.modes.chosen == mode) return old
            Sync.cancel(context)
            old.modes.chosen = mode
            old.shutdown()
            return synchronized(this) { build(context.applicationContext).also { instance = it; _current.value = it } }
        }
    }
}
