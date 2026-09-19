package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Mode

/**
 * The first thing a new install shows: where the engram lives. [fetch] is what
 * the phone's choice costs to download, said before it is chosen. Where this
 * build carries no core for the device the phone's choice is not drawn, rather
 * than drawn and refused.
 */
@Composable
fun ModeChooser(onPhoneOffered: Boolean, fetch: String, onPhone: () -> Unit, onServer: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.spacedBy(16.dp, androidx.compose.ui.Alignment.CenterVertically)) {
        Wordmark()
        Text("Where your engram lives", style = MaterialTheme.typography.titleLarge)
        if (onPhoneOffered) Choice("On this phone", "private · works offline · $fetch to fetch", onPhone)
        Choice("With a server", "pair with an engram you run", onServer)
    }
}

@Composable
private fun Choice(term: String, gloss: String, onClick: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.surface, shape = MaterialTheme.shapes.medium, modifier = Modifier.fillMaxWidth().clickable(onClick = onClick)) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(term, style = MaterialTheme.typography.titleMedium)
            Text(gloss, style = MaterialTheme.typography.bodySmall, color = muted())
        }
    }
}

/** Said once, where the mode is changed: the two bases are separate, and switching moves nothing. */
@Composable
fun SwitchDialog(to: Mode, onSwitch: () -> Unit, onStay: () -> Unit) {
    AlertDialog(
        onDismissRequest = onStay,
        title = { Text(if (to == Mode.server) "Switch to a server?" else "Switch to this phone?") },
        text = { Text("Separate bases · nothing is copied") },
        confirmButton = { TextButton(onClick = onSwitch) { Text("Switch") } },
        dismissButton = { TextButton(onClick = onStay) { Text("Stay") } },
    )
}
