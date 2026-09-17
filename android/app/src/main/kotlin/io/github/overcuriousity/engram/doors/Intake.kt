package io.github.overcuriousity.engram.doors

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.outbox.Incoming
import io.github.overcuriousity.engram.core.sync.Sync
import java.io.IOException

/** Every door lands here. Copies first, answers at once, kicks the worker. */
object Intake {
    suspend fun text(engram: Engram, text: String, title: String? = null, note: String? = null): String {
        val id = engram.outbox.enqueueText(text, title, note)
        Sync.kick(engram.app)
        return id
    }

    suspend fun uris(engram: Engram, uris: List<Uri>, title: String? = null, note: String? = null): String {
        val ctx = engram.app
        val incoming = uris.map { uri ->
            val mime = ctx.contentResolver.getType(uri)?.takeIf { it.isNotBlank() } ?: "application/octet-stream"
            Incoming(displayName(ctx, uri), mime) {
                ctx.contentResolver.openInputStream(uri) ?: throw IOException("cannot open $uri")
            }
        }
        val id = engram.outbox.enqueueFiles(incoming, title, note)
        Sync.kick(ctx)
        return id
    }

    private fun displayName(ctx: Context, uri: Uri): String =
        runCatching {
            ctx.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
                if (c.moveToFirst()) c.getString(0) else null
            }
        }.getOrNull() ?: uri.lastPathSegment ?: "file"
}
