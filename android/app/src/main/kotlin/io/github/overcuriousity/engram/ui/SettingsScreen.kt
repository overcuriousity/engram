package io.github.overcuriousity.engram.ui

import android.Manifest
import android.content.Intent
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Switch
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
import io.github.overcuriousity.engram.push.PushRegistrar
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

@Composable
fun SettingsScreen(engram: Engram, onJudging: (Screen) -> Unit = {}, onUnpair: () -> Unit) {
    val ctx = LocalContext.current
    val c by engram.store.current.collectAsStateWithLifecycle()
    val latest by engram.latestMoment.collectAsStateWithLifecycle(null)
    val pushFailure by engram.pushFailure.collectAsStateWithLifecycle()
    val keys by engram.store.pushKeysFlow.collectAsStateWithLifecycle()
    var noDistributor by remember { mutableStateOf(false) }
    var placeOn by remember { mutableStateOf(engram.placeOn) }
    var confirmUnpair by remember { mutableStateOf(false) }
    val askLocation = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { ok ->
        placeOn = ok
        engram.placeOn = ok
    }
    val askNotify = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {}
    LaunchedEffect(Unit) { askNotify.launch(Manifest.permission.POST_NOTIFICATIONS) }

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text("Settings", style = MaterialTheme.typography.titleLarge)
        Section("Server") {
            Line(c?.origin ?: "—")
            Line("version ${c?.serverVersion ?: "—"}", muted = true)
            Line(c?.deviceName ?: "", muted = true)
            Line(if (c?.pin != null) "pinned · ${c!!.pin!!.take(12)}…" else "public certificate · no pin", muted = true)
        }
        Section("Reminders") {
            val k = keys
            when {
                k != null -> Line("registered · ${k.distributor}", muted = true)
                noDistributor -> {
                    Line("No UnifiedPush distributor", muted = true)
                    Line("Reminders need one · ntfy or NextPush, from F-Droid", muted = true)
                    TextButton(onClick = {
                        ctx.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse("https://unifiedpush.org/users/distributors/")))
                    }) { Text("Distributors") }
                }
                else -> Button(onClick = { PushRegistrar.ensure(ctx, engram) { noDistributor = true } }) {
                    Text("Register for reminders")
                }
            }
            pushFailure?.let { Line("failed · $it", muted = true) }
            latest?.let { Line("last · ${it.title} · ${stamp(it.at)}", muted = true) }
        }
        Section("Place") {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.SpaceBetween,
                modifier = Modifier.fillMaxWidth(),
            ) {
                Column {
                    Line("Send my place")
                    Line("a one-kilometre cell · never finer", muted = true)
                }
                Switch(checked = placeOn, onCheckedChange = { on ->
                    if (on) askLocation.launch(Manifest.permission.ACCESS_COARSE_LOCATION)
                    else { placeOn = false; engram.placeOn = false }
                })
            }
        }
        // Where judging lives, and the only place it is offered from. Not a
        // tab, not a section on home, and no count until somebody has opened
        // the screen that fetched one.
        Section("Judging") {
            val pairs by JudgeCounts.pairs.collectAsStateWithLifecycle()
            val gaps by JudgeCounts.gaps.collectAsStateWithLifecycle()
            val aside by JudgeCounts.setAside.collectAsStateWithLifecycle()
            JudgeLine("Duplicate pairs", pairs) { onJudging(Screen.Pairs) }
            JudgeLine("Gaps", gaps) { onJudging(Screen.Gaps) }
            JudgeLine("While you were away", aside) { onJudging(Screen.Journal) }
        }
        Section("This phone") {
            Line(engram.situation.bundle(placeOn).toString(), muted = true, mono = true)
        }
        OutlinedButton(
            onClick = { confirmUnpair = true },
            colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error),
        ) { Text("Unpair") }
    }
    if (confirmUnpair) AlertDialog(
        onDismissRequest = { confirmUnpair = false },
        title = { Text("Unpair this phone?") },
        text = { Text("Queued captures stay until you pair again.") },
        confirmButton = {
            TextButton(onClick = { confirmUnpair = false; PushRegistrar.forget(ctx); onUnpair() }) { Text("Unpair") }
        },
        dismissButton = { TextButton(onClick = { confirmUnpair = false }) { Text("Keep") } },
    )
}

@Composable
private fun JudgeLine(label: String, count: Int?, onClick: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(vertical = 6.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Line(label)
        // What was actually fetched, once, and nothing when nothing has been.
        count?.let { Line(it.toString(), muted = true) }
    }
}

@Composable
private fun Section(title: String, content: @Composable ColumnScope.() -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Text(title, style = MaterialTheme.typography.titleMedium)
        content()
    }
}

@Composable
private fun Line(text: String, muted: Boolean = false, mono: Boolean = false) {
    Text(
        text,
        style = if (mono) MaterialTheme.typography.labelMedium else MaterialTheme.typography.bodyMedium,
        color = if (muted) muted() else MaterialTheme.colorScheme.onBackground,
    )
}

private fun stamp(at: Long) =
    DateTimeFormatter.ofPattern("dd.MM., HH:mm").format(Instant.ofEpochSecond(at).atZone(ZoneId.systemDefault()))
