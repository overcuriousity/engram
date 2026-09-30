package io.github.overcuriousity.engram.doors

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.core.content.IntentCompat
import androidx.lifecycle.lifecycleScope
import io.github.overcuriousity.engram.App
import io.github.overcuriousity.engram.MainActivity
import kotlinx.coroutines.launch

/**
 * Every share lands here and leaves at once: it copies, enqueues, toasts, and
 * finishes — the sending app never sees a screen of ours. Unpaired, the share
 * is kept all the same and the app opens on pairing; the outbox delivers it
 * once there is a server, which is what `Engram.pair` kicks for.
 */
class ShareActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val engram = (application as App).engram
        // Contained, there is always somewhere to deliver it: the worker starts
        // the core. In server mode with no pairing there is not yet, and a
        // share used to be dropped with a toast — kept here instead, owed.
        val unpaired = !engram.loopback && engram.store.current.value == null
        val i = intent
        val title = i.getStringExtra(Intent.EXTRA_SUBJECT) ?: i.getStringExtra(Intent.EXTRA_TITLE)
        lifecycleScope.launch {
            try {
                when (i.action) {
                    Intent.ACTION_PROCESS_TEXT -> {
                        val t = i.getCharSequenceExtra(Intent.EXTRA_PROCESS_TEXT)?.toString().orEmpty()
                        if (t.isNotBlank()) Intake.text(engram, t)
                    }
                    Intent.ACTION_SEND -> {
                        val stream = IntentCompat.getParcelableExtra(i, Intent.EXTRA_STREAM, Uri::class.java)
                        val text = i.getStringExtra(Intent.EXTRA_TEXT)
                        when {
                            stream != null -> Intake.uris(engram, listOf(stream), title, text)
                            !text.isNullOrBlank() -> Intake.text(engram, text, title)
                        }
                    }
                    Intent.ACTION_SEND_MULTIPLE -> {
                        val streams = IntentCompat.getParcelableArrayListExtra(i, Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
                        if (streams.isNotEmpty()) Intake.uris(engram, streams, title, i.getStringExtra(Intent.EXTRA_TEXT))
                    }
                }
                if (unpaired) {
                    Toast.makeText(this@ShareActivity, "Kept on this phone · pair engram to deliver it", Toast.LENGTH_LONG).show()
                    startActivity(Intent(this@ShareActivity, MainActivity::class.java))
                } else {
                    Toast.makeText(this@ShareActivity, "Kept · engram", Toast.LENGTH_SHORT).show()
                }
            } catch (e: Exception) {
                Toast.makeText(this@ShareActivity, "Could not read that: ${e.message}", Toast.LENGTH_LONG).show()
            } finally {
                finish()
            }
        }
    }
}
