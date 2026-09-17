package io.github.overcuriousity.engram

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import io.github.overcuriousity.engram.core.LightSample
import io.github.overcuriousity.engram.ui.EngramApp
import io.github.overcuriousity.engram.ui.EngramTheme
import io.github.overcuriousity.engram.ui.Screen

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val app = application as App
        val pairText = intent?.data?.toString()?.takeIf { it.startsWith("engram://pair") }
        val start = when (intent?.getStringExtra("screen")) {
            "queue" -> Screen.Queue
            "settings" -> Screen.Settings
            else -> null
        }
        setContent { EngramTheme { EngramApp(app.engram, start, pairText, onUnpair = app::unpairAsync) } }
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
