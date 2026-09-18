package io.github.overcuriousity.engram

import android.app.Application
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.reminders.LocalReminders
import io.github.overcuriousity.engram.push.Alarms
import io.github.overcuriousity.engram.push.Reminders
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

class App : Application() {
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    /** Whichever instance is in use. Asked for each time, because changing the mode replaces it. */
    val engram: Engram get() = Engram.get(this)

    override fun onCreate() {
        super.onCreate()
        Engram.get(this)
        Reminders.ensureChannel(this)
        LocalReminders.ringer = Alarms::set
    }

    fun unpairAsync() {
        scope.launch { engram.unpair() }
    }
}
