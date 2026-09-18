package io.github.overcuriousity.engram.core

import android.content.SharedPreferences
import io.github.overcuriousity.engram.core.contained.ModelManifest
import io.github.overcuriousity.engram.core.contained.Models
import io.github.overcuriousity.engram.core.contained.Role
import java.io.File

/** Where the engram this app shows lives: on a server it is paired with, or in this process. */
enum class Mode { server, contained }

/** The one stored value. Null until somebody chooses, and an install that never chose is a server's client. */
class ModeStore(private val prefs: SharedPreferences) {
    var chosen: Mode?
        get() = prefs.getString("mode", null)?.let { w -> Mode.entries.firstOrNull { it.name == w } }
        set(v) = prefs.edit().putString("mode", v?.name).apply()

    /** How a contained phone answers questions. Unset is `device`: a model that is installed is used. */
    var ask: AskVia
        get() = prefs.getString("ask", null)?.let { w -> AskVia.entries.firstOrNull { it.name == w } } ?: AskVia.device
        set(v) = prefs.edit().putString("ask", v.name).apply()
}

/** With the model on this phone, through an endpoint of the person's choosing, or not at all. */
enum class AskVia { device, endpoint, off }

/**
 * Everything a mode keeps on this phone. The two share no path, which is the
 * whole of how one mode's outbox is never drained into the other.
 *
 * Server mode's places are the ones it had before there were modes: its outbox
 * exists nowhere else, and is not moved to tidy a directory.
 */
class ModeState(val dbName: String, val outbox: File, val core: File?, val models: File?) {
    /**
     * The model file for each role, where a whole one is here. A file under
     * its final name has been verified — the downloader gives it that name
     * last — and the length is checked again because it is free. Where both
     * ask models are installed the larger wins: nobody has it by accident.
     */
    fun models(): Models {
        fun at(role: Role) = ModelManifest.all.filter { it.role == role }.sortedBy { it.default }
            .firstNotNullOfOrNull { m -> models?.let { File(it, m.file) }?.takeIf { f -> f.isFile && f.length() == m.bytes }?.path }
        return Models(embed = at(Role.embed), rerank = at(Role.rerank), ask = at(Role.ask))
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
