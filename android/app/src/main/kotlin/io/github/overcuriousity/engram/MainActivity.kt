package io.github.overcuriousity.engram

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import io.github.overcuriousity.engram.core.LightSample
import io.github.overcuriousity.engram.ui.EngramApp
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import io.github.overcuriousity.engram.core.Engram
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.ui.EngramTheme
import io.github.overcuriousity.engram.ui.Screen
import io.github.overcuriousity.engram.ui.ThemeMode

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val app = application as App
        val pairText = intent?.data?.toString()?.takeIf { it.startsWith("engram://pair") }
        val asked = intent?.getStringExtra("screen")
        val start = when (asked) {
            "queue" -> Screen.Queue
            "settings" -> Screen.Settings
            else -> null
        }
        // The tile and the launcher shortcut are a capture in one press. There
        // is no capture screen to send them to any more, so they open home
        // with the box focused and the keyboard already up. "compose" is what
        // a shortcut pinned by an older build still says.
        val focusBox = asked == "capture" || asked == "compose"
        setContent {
            // Changing the mode replaces the engram. Keyed on the instance, the
            // whole composition goes with the one it was remembered from.
            val current by Engram.current.collectAsStateWithLifecycle()
            val engram = current ?: app.engram
            val theme by engram.theme.collectAsStateWithLifecycle()
            EngramTheme(mode = ThemeMode.entries.firstOrNull { it.name.equals(theme, ignoreCase = true) } ?: ThemeMode.System) {
                key(engram) { EngramApp(engram, start, pairText, focusBox, onUnpair = app::unpairAsync) }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        recreate()
    }

    override fun onResume() {
        super.onResume()
        LightSample.start(this)
        (application as App).engram.counters.mark()
    }

    override fun onPause() {
        super.onPause()
        LightSample.stop(this)
    }
}
