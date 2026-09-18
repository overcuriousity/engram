package io.github.overcuriousity.engram.core.contained

import io.github.overcuriousity.engram.core.Connection
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.io.File

sealed interface CoreState {
    data object Idle : CoreState
    data object Starting : CoreState
    data class Running(val connection: Connection) : CoreState
    data class Unavailable(val why: String) : CoreState
}

/**
 * The core, started when something first needs it, and the connection to it.
 *
 * That connection is a port and a token good for this launch. It lives here,
 * in memory, and is never handed to `ConnectionStore`: written down, it would
 * be a dead origin and a revoked token by the next start.
 */
class Contained(
    private val dataDir: File,
    private val models: () -> Models,
    private val deviceName: String,
    private val boot: (String, Models) -> Started = Core::start,
) {
    private val gate = Mutex()
    private val _state = MutableStateFlow<CoreState>(CoreState.Idle)
    val state: StateFlow<CoreState> get() = _state
    private val _connected = MutableStateFlow<Connection?>(null)
    val connected: StateFlow<Connection?> get() = _connected

    /** The connection, starting the core if it is not running. Null where it cannot be; [state] says why. */
    suspend fun ensure(): Connection? = gate.withLock {
        _connected.value?.let { return it }
        _state.value = CoreState.Starting
        // Throwable, not Exception: a library that does not link is an Error.
        val started = withContext(Dispatchers.IO) { runCatching { boot(dataDir.path, models()) } }
        started.fold(
            onSuccess = {
                val c = Connection("http://127.0.0.1:${it.port}", it.token, pin = null, serverVersion = "", deviceName = deviceName)
                _connected.value = c
                _state.value = CoreState.Running(c)
                c
            },
            onFailure = {
                _state.value = CoreState.Unavailable(it.message ?: it.javaClass.simpleName)
                null
            },
        )
    }
}
