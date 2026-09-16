package io.github.overcuriousity.engram.core

import android.content.Context
import io.github.overcuriousity.engram.core.db.Db
import io.github.overcuriousity.engram.core.outbox.Drainer
import io.github.overcuriousity.engram.core.outbox.Outbox
import kotlinx.coroutines.flow.MutableStateFlow
import java.io.File

/** Everything the app and its receivers are allowed to touch, built once. Filled in by Task 8. */
class Engram private constructor(val app: Context) {
    val db = Db.open(app)
    val outbox = Outbox(db, File(app.filesDir, "outbox"))
    val refused = MutableStateFlow(false)
    val pinMismatch = MutableStateFlow<PinMismatch?>(null)
    internal fun drainer(): Drainer? = null

    companion object {
        @Volatile private var instance: Engram? = null
        fun get(context: Context): Engram =
            instance ?: synchronized(this) { instance ?: Engram(context.applicationContext).also { instance = it } }
    }
}
