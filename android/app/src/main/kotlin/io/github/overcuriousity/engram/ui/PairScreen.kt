package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.ClaimRefused
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.PairUri
import kotlinx.coroutines.launch

/** The app's minimum server. A QR from an older one is offered, not refused. */
private const val MIN_SERVER = "0.1.0"

@Composable
fun PairScreen(engram: Engram, initialText: String?, knownOrigin: String? = null) {
    val scope = rememberCoroutineScope()
    var text by remember { mutableStateOf(initialText ?: "") }
    var busy by remember { mutableStateOf<String?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var oldServer by remember { mutableStateOf<PairUri?>(null) }

    fun claim(uri: PairUri) {
        busy = uri.origin
        error = null
        scope.launch {
            try {
                engram.pair(uri)
            } catch (e: ClaimRefused) {
                error = "Code expired or used · press the button again"
            } catch (e: Exception) {
                error = e.message ?: "Could not reach ${uri.origin}"
            } finally {
                busy = null
            }
        }
    }

    fun take(t: String) {
        val uri = PairUri.parse(t)
        if (uri == null) {
            error = "Not an engram pairing code"
            return
        }
        if (compareVersions(uri.serverVersion, MIN_SERVER) < 0) oldServer = uri else claim(uri)
    }
    LaunchedEffect(initialText) { if (!initialText.isNullOrBlank()) take(initialText) }

    Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Wordmark()
        Text("Pair", style = MaterialTheme.typography.titleLarge)
        Text(
            if (knownOrigin != null) "Open $knownOrigin/ui/app and press the button" else "Open Settings → Pair the app on your engram",
            style = MaterialTheme.typography.bodySmall, color = muted(),
        )
        if (busy == null) Scanner(onText = ::take)
        OutlinedTextField(
            value = text, onValueChange = { text = it }, label = { Text("or paste the code") },
            modifier = Modifier.fillMaxWidth(),
        )
        Button(onClick = { take(text) }, enabled = busy == null && text.isNotBlank()) { Text("Pair") }
        busy?.let { Text("Pairing with $it…", style = MaterialTheme.typography.bodySmall) }
        error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
    }

    oldServer?.let { uri ->
        AlertDialog(
            onDismissRequest = { oldServer = null },
            title = { Text("Server ${uri.serverVersion} · app expects $MIN_SERVER") },
            text = { Text("Some things may not work.") },
            confirmButton = { TextButton(onClick = { oldServer = null; claim(uri) }) { Text("Pair anyway") } },
            dismissButton = { TextButton(onClick = { oldServer = null }) { Text("Cancel") } },
        )
    }
}

/** Dotted numeric compare; anything unparseable is 0. */
internal fun compareVersions(a: String, b: String): Int {
    val pa = a.split('.').map { it.toIntOrNull() ?: 0 }
    val pb = b.split('.').map { it.toIntOrNull() ?: 0 }
    for (i in 0 until maxOf(pa.size, pb.size)) {
        val d = (pa.getOrNull(i) ?: 0) - (pb.getOrNull(i) ?: 0)
        if (d != 0) return d
    }
    return 0
}
