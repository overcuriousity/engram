package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

enum class Tone { Ok, Warn, Error }

/** The manifest's notice: status symbol and words between hairlines, no box. */
@Composable
fun Notice(head: String, body: String = "", tone: Tone = Tone.Warn, actions: @Composable () -> Unit = {}) {
    val (symbol, colour) = when (tone) {
        Tone.Ok -> "✓" to MaterialTheme.colorScheme.tertiary
        Tone.Warn -> "⚠" to MaterialTheme.colorScheme.secondary
        Tone.Error -> "✗" to MaterialTheme.colorScheme.error
    }
    Column(Modifier.fillMaxWidth().padding(16.dp, 6.dp)) {
        HorizontalDivider(color = MaterialTheme.colorScheme.outline)
        Row(Modifier.padding(vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("$symbol $head", style = MaterialTheme.typography.labelLarge, color = colour)
                if (body.isNotEmpty()) Text(body, style = MaterialTheme.typography.bodySmall, color = muted())
            }
            actions()
        }
        HorizontalDivider(color = MaterialTheme.colorScheme.outline)
    }
}
