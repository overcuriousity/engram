package io.github.overcuriousity.engram.core

import android.content.SharedPreferences
import io.github.overcuriousity.engram.core.contained.Models
import java.io.File

/** Where the engram this app shows lives: on a server it is paired with, or in this process. */
enum class Mode { server, contained }

/** The one stored value. Null until somebody chooses, and an install that never chose is a server's client. */
class ModeStore(private val prefs: SharedPreferences) {
    var chosen: Mode?
        get() = prefs.getString("mode", null)?.let { w -> Mode.entries.firstOrNull { it.name == w } }
        set(v) = prefs.edit().putString("mode", v?.name).apply()
}

/**
 * Everything a mode keeps on this phone. The two share no path, which is the
 * whole of how one mode's outbox is never drained into the other.
 *
 * Server mode's places are the ones it had before there were modes: its outbox
 * exists nowhere else, and is not moved to tidy a directory.
 */
class ModeState(val dbName: String, val outbox: File, val core: File?, val models: File?) {
    /** The model file for each role, where there is one. Part 5's manifest replaces the names. */
    fun models(): Models {
        fun at(name: String) = models?.let { File(it, name) }?.takeIf { it.isFile }?.path
        return Models(embed = at("embed.gguf"), rerank = at("rerank.gguf"), ask = at("ask.gguf"))
    }

    companion object {
        fun of(mode: Mode, filesDir: File): ModeState = when (mode) {
            Mode.server -> ModeState("engram.db", File(filesDir, "outbox"), core = null, models = null)
            Mode.contained -> File(filesDir, "contained").let {
                ModeState("contained.db", File(it, "outbox"), File(it, "core"), File(it, "models"))
            }
        }
    }
}
