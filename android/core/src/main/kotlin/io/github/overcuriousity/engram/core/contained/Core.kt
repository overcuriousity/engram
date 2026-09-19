package io.github.overcuriousity.engram.core.contained

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/** Where a started core listens, and the word that opens it for this launch. */
@Serializable
data class Started(val port: Int, val token: String)

/** The model file for each role that runs on this device. Absent: that role does not. */
@Serializable
data class Models(val embed: String? = null, val rerank: String? = null, val ask: String? = null, val speech: String? = null)

/** An OpenAI-compatible endpoint of the person's choosing, for ask where the phone carries no model for it. */
@Serializable
data class Endpoint(val baseUrl: String, val model: String, val apiKey: String? = null)

/** What a launch is given: the models on the device, and where to ask when there is no file for that. */
@Serializable
data class Setup(
    val embed: String? = null,
    val rerank: String? = null,
    val ask: String? = null,
    val askEndpoint: Endpoint? = null,
    /** A whisper.cpp model. Where there is one, the core opens the microphone's door. */
    val speech: String? = null,
) {
    constructor(models: Models, askEndpoint: Endpoint? = null) : this(models.embed, models.rerank, models.ask, askEndpoint, models.speech)
}

class CoreFailed(message: String) : Exception(message)

/**
 * The engram that lives in this process. Two calls wide on purpose: everything
 * it can do is reached over the same HTTP API the server speaks, so nothing
 * above `core` has a second way to ask.
 */
object Core {
    private val json = Json { ignoreUnknownKeys = true }

    @Serializable
    private data class Answer(val port: Int? = null, val token: String? = null, val error: String? = null, val open: Boolean? = null)

    /**
     * Whether this build carries the core for this device. The library is
     * built for arm64-v8a alone, so on anything else — an x86_64 emulator, a
     * checkout built without Rust — contained mode is absent rather than
     * broken. Asked once; a load that failed is not going to succeed later.
     */
    val available: Boolean by lazy { runCatching { System.loadLibrary("engram_android") }.isSuccess }

    private val initialised = java.util.concurrent.atomic.AtomicBoolean(false)

    /**
     * Gives the core's certificate verifier the app's Context, once. The core
     * asks Android whether a server's chain is trusted, and without this its
     * first HTTPS request — a shared link, an ask endpoint — panics.
     */
    fun init(context: android.content.Context) {
        if (!available) throw CoreFailed("not built for this device")
        if (!initialised.compareAndSet(false, true)) return
        val a = json.decodeFromString<Answer>(init(context.applicationContext as Any))
        if (a.error != null) { initialised.set(false); throw CoreFailed(a.error) }
    }

    /** Starts the core over [dataDir], or answers the one already running. Blocks; call off the main thread. */
    fun start(dataDir: String, setup: Setup): Started {
        if (!available) throw CoreFailed("not built for this device")
        val a = json.decodeFromString<Answer>(start(dataDir, json.encodeToString(Setup.serializer(), setup)))
        if (a.error != null || a.port == null || a.token == null) throw CoreFailed(a.error ?: "the core answered nothing")
        return Started(a.port, a.token)
    }

    /** Stops it and waits for a job in flight. Safe to call when nothing runs. */
    fun shutdown() {
        if (available) stop()
    }

    /**
     * Says whether model work may run now, and answers whether it will: the
     * core opens its queue to that work only where an endpoint exists to do
     * it. False where nothing runs.
     */
    fun background(allow: Boolean): Boolean =
        available && runCatching { json.decodeFromString<Answer>(background0(allow)).open }.getOrNull() == true

    @JvmStatic @JvmName("background") private external fun background0(allow: Boolean): String

    @JvmStatic private external fun init(context: Any): String

    @JvmStatic private external fun start(dataDir: String, setup: String): String

    @JvmStatic private external fun stop(): String
}
