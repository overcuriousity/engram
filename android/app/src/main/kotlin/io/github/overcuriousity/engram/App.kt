package io.github.overcuriousity.engram

import android.app.Application
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.push.Reminders
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

class App : Application() {
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    lateinit var engram: Engram

    override fun onCreate() {
        super.onCreate()
        engram = Engram.get(this)
        Reminders.ensureChannel(this)
    }

    fun unpairAsync() {
        scope.launch { engram.unpair() }
    }
}
