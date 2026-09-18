package io.github.overcuriousity.engram.core.contained

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/** Where a started core listens, and the word that opens it for this launch. */
@Serializable
data class Started(val port: Int, val token: String)

/** The model file for each role that runs on this device. Absent: that role does not. */
@Serializable
data class Models(val embed: String? = null, val rerank: String? = null, val ask: String? = null)

class CoreFailed(message: String) : Exception(message)

/**
 * The engram that lives in this process. Two calls wide on purpose: everything
 * it can do is reached over the same HTTP API the server speaks, so nothing
 * above `core` has a second way to ask.
 */
object Core {
    private val json = Json { ignoreUnknownKeys = true }

    @Serializable
    private data class Answer(val port: Int? = null, val token: String? = null, val error: String? = null)

    init {
        System.loadLibrary("engram_android")
    }

    /** Starts the core over [dataDir], or answers the one already running. Blocks; call off the main thread. */
    fun start(dataDir: String, models: Models): Started {
        val a = json.decodeFromString<Answer>(start(dataDir, json.encodeToString(Models.serializer(), models)))
        if (a.error != null || a.port == null || a.token == null) throw CoreFailed(a.error ?: "the core answered nothing")
        return Started(a.port, a.token)
    }

    /** Stops it and waits for a job in flight. Safe to call when nothing runs. */
    fun shutdown() {
        stop()
    }

    @JvmStatic private external fun start(dataDir: String, models: String): String

    @JvmStatic private external fun stop(): String
}
