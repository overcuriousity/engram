package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * The first visit to Ask on a phone with nothing to answer with: fetch a
 * model, point at an endpoint, or leave the door shut. Offered in place, and
 * only here — never a banner, never on home.
 */
@Composable
fun AskOfferPane(models: @Composable () -> Unit, onEndpoint: () -> Unit, onOff: () -> Unit) {
    Column(Modifier.fillMaxWidth().padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text("Ask · no model yet", style = MaterialTheme.typography.titleMedium)
        models()
        Column(Modifier.fillMaxWidth().clickable(onClick = onEndpoint).padding(vertical = 6.dp)) {
            Text("Use an endpoint", style = MaterialTheme.typography.bodyMedium)
            Text("any OpenAI-compatible server", style = MaterialTheme.typography.bodySmall, color = muted())
        }
        TextButton(onClick = onOff) { Text("Leave Ask off") }
    }
}

@Composable
fun AskOffPane(onSettings: () -> Unit) {
    Column(Modifier.fillMaxWidth().padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text("Ask · off", style = MaterialTheme.typography.titleMedium)
        TextButton(onClick = onSettings) { Text("Settings") }
    }
}
