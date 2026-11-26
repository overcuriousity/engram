package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
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
fun PairScreen(engram: Engram, initialText: String?, knownOrigin: String? = null, onPaired: () -> Unit = {}) {
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
                // A phone that was already paired stays on this screen, and
                // its scanner would read the same, now spent, code again.
                onPaired()
            } catch (e: ClaimRefused) {
                error = "Code expired or used · press the button again"
            } catch (e: Exception) {
                error = e.message ?: "Could not reach ${uri.origin}"
            } finally {
                busy = null
            }
        }
    }

    var offered by remember { mutableStateOf<PairUri?>(null) }

    fun act(arrival: Arrival) {
        when (arrival) {
            Arrival.NotACode -> error = "Not an engram pairing code"
            is Arrival.Offer -> offered = arrival.uri
            is Arrival.Old -> oldServer = arrival.uri
            is Arrival.Claim -> claim(arrival.uri)
        }
    }

    fun take(t: String) = act(arrival(t, byLink = false, knownOrigin = knownOrigin))
    // `initialText` is what an intent carried: any page or app can send one,
    // so it is claimed only after a press, here or before the mode changed.
    LaunchedEffect(initialText) {
        if (!initialText.isNullOrBlank()) {
            act(arrival(initialText, byLink = PairConsent.code != initialText, knownOrigin = knownOrigin))
        }
    }

    Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Wordmark()
        Text("Pair", style = MaterialTheme.typography.titleLarge)
        Text(
            if (knownOrigin != null) "Open $knownOrigin/ui/app and press the button" else "Open Settings → Pair the app on your engram",
            style = MaterialTheme.typography.bodySmall, color = muted(),
        )
        // The scanner takes what is left between the heading and the two ways
        // in at the bottom: the screen is a viewfinder, and the paste field is
        // the way through when the code is on something that cannot be held up
        // to a camera.
        if (busy == null) Scanner(Modifier.fillMaxWidth().weight(1f), onText = ::take) else Spacer(Modifier.weight(1f))
        OutlinedTextField(
            value = text, onValueChange = { text = it }, label = { Text("or paste the code") },
            modifier = Modifier.fillMaxWidth(),
        )
        Button(onClick = { take(text) }, enabled = busy == null && text.isNotBlank()) { Text("Pair") }
        busy?.let { Text("Pairing with $it…", style = MaterialTheme.typography.bodySmall) }
        error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
    }

    offered?.let { uri ->
        PairOffer(uri, knownOrigin?.takeIf { it != uri.origin }, onPair = { offered = null; act(pressed(uri)) }, onCancel = { offered = null })
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

/** What becomes of a pairing code. */
internal sealed interface Arrival {
    data object NotACode : Arrival
    /** Shown with its origin; nothing is claimed before a press. */
    data class Offer(val uri: PairUri) : Arrival
    data class Old(val uri: PairUri) : Arrival
    data class Claim(val uri: PairUri) : Arrival
}

/**
 * A code that came by link, or that names another server than the one held,
 * waits for a press: claiming it hands every later capture to its origin.
 */
internal fun arrival(text: String, byLink: Boolean, knownOrigin: String?): Arrival {
    val uri = PairUri.parse(text) ?: return Arrival.NotACode
    if (byLink || (knownOrigin != null && knownOrigin != uri.origin)) return Arrival.Offer(uri)
    return pressed(uri)
}

/** After the press, or where none was needed. */
internal fun pressed(uri: PairUri): Arrival =
    if (compareVersions(uri.serverVersion, MIN_SERVER) < 0) Arrival.Old(uri) else Arrival.Claim(uri)

/** The code a press already answered for, so changing the mode does not ask twice. */
internal object PairConsent {
    @Volatile var code: String? = null
}

@Composable
internal fun PairOffer(uri: PairUri, replaces: String?, onPair: () -> Unit, onCancel: () -> Unit) {
    AlertDialog(
        onDismissRequest = onCancel,
        title = { Text(uri.origin) },
        text = { if (replaces != null) Text("Replaces $replaces") },
        confirmButton = { TextButton(onClick = onPair) { Text("Pair") } },
        dismissButton = { TextButton(onClick = onCancel) { Text("Cancel") } },
    )
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
