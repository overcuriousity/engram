package io.github.overcuriousity.engram.ui

import android.content.Intent
import android.net.Uri
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.contained.Downloads
import io.github.overcuriousity.engram.core.contained.Model
import io.github.overcuriousity.engram.core.contained.Progress

/**
 * One model: what it is, how large, whose terms, and the one thing that can be
 * done about it now. Handed everything it shows, so it is drawn the same in a
 * test as on a phone.
 */
@Composable
fun ModelRow(
    model: Model,
    progress: Progress,
    installed: Boolean,
    onDownload: () -> Unit,
    onCancel: () -> Unit,
    onRemove: () -> Unit,
    onTerms: (String) -> Unit = {},
) {
    Column(Modifier.fillMaxWidth().padding(vertical = 6.dp), verticalArrangement = Arrangement.spacedBy(2.dp)) {
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(model.name, style = MaterialTheme.typography.bodyMedium)
                val gloss = "${sizeWords(model.bytes)} · ${model.licence}"
                val terms = model.terms
                Text(
                    gloss, style = MaterialTheme.typography.bodySmall, color = muted(),
                    modifier = if (terms != null) Modifier.clickable { onTerms(terms) } else Modifier,
                )
            }
            when {
                installed -> { Text("Installed", style = MaterialTheme.typography.bodySmall, color = muted()); TextButton(onClick = onRemove) { Text("Remove") } }
                progress.state == Progress.State.Running -> TextButton(onClick = onCancel) { Text("Cancel") }
                progress.state == Progress.State.Waiting -> TextButton(onClick = onCancel) { Text("Cancel") }
                progress.state == Progress.State.Failed -> TextButton(onClick = onDownload) { Text("Retry") }
                else -> TextButton(onClick = onDownload) { Text("Download") }
            }
        }
        when (progress.state) {
            Progress.State.Running -> if (!installed) {
                LinearProgressIndicator(progress = { progress.bytes.toFloat() / progress.of.coerceAtLeast(1) }, modifier = Modifier.fillMaxWidth())
                Text("${sizeWords(progress.bytes)} of ${sizeWords(progress.of)}", style = MaterialTheme.typography.bodySmall, color = muted())
            }
            Progress.State.Waiting -> if (!installed) Text("Waiting for Wi-Fi", style = MaterialTheme.typography.bodySmall, color = muted())
            Progress.State.Failed -> if (!installed) Text("failed · ${progress.error ?: "unknown"}", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
            else -> {}
        }
    }
}

@Composable
fun MeteredDialog(size: String, onAnyway: () -> Unit, onWait: () -> Unit, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Metered network · $size") },
        confirmButton = { TextButton(onClick = onAnyway) { Text("Download anyway") } },
        dismissButton = { TextButton(onClick = onWait) { Text("Wait for Wi-Fi") } },
    )
}

/**
 * A [ModelRow] over the real download. [onChanged] is told when the model
 * arrives or goes, which is when its caller has something to re-check: what
 * is still missing, or a core to restart onto what is there now.
 */
@Composable
fun ModelLine(engram: Engram, model: Model, onChanged: () -> Unit) {
    val ctx = LocalContext.current
    val progress by remember(model) { Downloads.progress(ctx, model) }
        .collectAsStateWithLifecycle(Progress(0, model.bytes, Progress.State.Idle))
    var installed by remember(model) { mutableStateOf(engram.installed(model)) }
    var metered by remember { mutableStateOf(false) }
    LaunchedEffect(progress.state) {
        if (progress.state == Progress.State.Done && !installed) { installed = engram.installed(model); if (installed) onChanged() }
    }
    ModelRow(
        model, progress, installed,
        onDownload = { if (engram.metered) metered = true else Downloads.start(ctx, model, allowMetered = false) },
        onCancel = { Downloads.cancel(ctx, model) },
        onRemove = { engram.downloader?.remove(model); installed = false; onChanged() },
        onTerms = { ctx.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(it))) },
    )
    if (metered) MeteredDialog(
        sizeWords(model.bytes),
        onAnyway = { metered = false; Downloads.start(ctx, model, allowMetered = true) },
        onWait = { metered = false; Downloads.start(ctx, model, allowMetered = false) },
        onDismiss = { metered = false },
    )
}
