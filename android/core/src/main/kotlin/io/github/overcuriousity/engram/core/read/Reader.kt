package io.github.overcuriousity.engram.core.read

import kotlinx.coroutines.flow.Flow

/** Whether the source of a read could be reached, and whether it let us in. */
enum class Reach {
    /** The source answered. What it said may still be a no — see [Read.error]. */
    Fresh,

    /** The source could not be reached. Whatever is shown is what was fetched before. */
    Unreachable,

    /** The source was reached and refused the credential: paired, and unpaired on the far side. */
    Refused,
}

/**
 * One state of a read: what there is to show, when it was last known to be
 * current, and where the source stands. A screen draws this and nothing else.
 */
data class Read<out T>(
    val value: T?,
    val fetchedAt: Long?,
    val reach: Reach,
    val loading: Boolean,
    /** What the source said when it said no, in its own words. */
    val error: String? = null,
)

/** What is being asked for. The key is the path and its query in one fixed order. */
data class Request(val path: String, val query: Map<String, String?> = emptyMap()) {
    val key: String
        get() {
            val q = query.filterValues { it != null }.toSortedMap().entries.joinToString("&") { "${it.key}=${it.value}" }
            return if (q.isEmpty()) path else "$path?$q"
        }
}

/**
 * THE SEAM. Every screen, every receiver and later the watch ask this for what
 * they show, and none of them learns where an answer came from.
 *
 * Today there is one implementation, [ServerReader]: the paired server, with
 * each answer kept so that it can be shown again, marked with when it was
 * fetched, while the server cannot be reached.
 *
 * A later version of the app is self-contained — an engram on the device,
 * replacing the server by default. That version is a second implementation of
 * this interface, chosen where `Engram` builds its reader. If that is ever not
 * true, something above `core` has named an implementation or a URL; nothing
 * is allowed to. Writes have the same seam on the other side: the outbox.
 */
interface Reader {
    /**
     * What is held, at once, then what the source says. Emits one or two
     * states and completes; a screen collects again to retry.
     */
    fun <T> read(request: Request, decode: (String) -> T): Flow<Read<T>>

    /**
     * A question whose answer belongs to the moment it was asked in — the
     * offer for this situation. Asked of the source every time and never kept.
     */
    suspend fun <T> ask(path: String, json: String, decode: (String) -> T): Read<T> = call("POST", path, json, decode)

    /**
     * A write pressed where the source is, answered in words: a verdict, a
     * kept answer, a saved setting. Not an owed write — the outbox is for
     * those — because the answer is what the screen shows next, and a press
     * with no server to answer it is said to be one. A `204` decodes an
     * empty body.
     */
    suspend fun <T> call(method: String, path: String, json: String?, decode: (String) -> T): Read<T>

    /** Something the source is told and nothing waits on. Failure is silent. */
    suspend fun tell(path: String, json: String)

    /**
     * A recording in, the words in it back. Belongs to the moment like [ask]
     * does and is never kept. Where the source has no speech model the answer
     * is an error, not a value; `Api.status()` says so in advance.
     *
     * On the seam rather than beside it because a self-contained app hears on
     * the device, and the screen holding the microphone must not know which.
     */
    suspend fun hear(audio: ByteArray, mime: String): Read<String>
}
