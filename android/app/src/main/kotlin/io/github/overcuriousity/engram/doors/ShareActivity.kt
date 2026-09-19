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
 * Every share lands here and leaves at once. Unpaired, it says so and opens
 * the app; paired, it copies, enqueues, toasts, and finishes — the sending
 * app never sees a screen of ours.
 */
class ShareActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val engram = (application as App).engram
        // Contained, there is always somewhere to keep it: the worker starts
        // the core to deliver what was shared.
        if (!engram.loopback && engram.store.current.value == null) {
            Toast.makeText(this, "Pair engram first", Toast.LENGTH_SHORT).show()
            startActivity(Intent(this, MainActivity::class.java))
            finish()
            return
        }
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
                Toast.makeText(this@ShareActivity, "Kept · engram", Toast.LENGTH_SHORT).show()
            } catch (e: Exception) {
                Toast.makeText(this@ShareActivity, "Could not read that: ${e.message}", Toast.LENGTH_LONG).show()
            } finally {
                finish()
            }
        }
    }
}
