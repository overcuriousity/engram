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
import androidx.compose.material3.FilterChip
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import kotlinx.coroutines.launch
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
fun SettingsScreen(engram: Engram, onJudging: (Screen) -> Unit = {}, onQueue: () -> Unit = {}, onUnpair: () -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
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
        // Which language captures are read and written in. It applies to what
        // is captured from now on; nothing already stored is rewritten.
        Section("Language") {
            val lang = rememberRead(engram, Api.lang(), Decode.lang)
            var choosing by remember { mutableStateOf(false) }
            var chosen by remember(lang.read.value?.chosen) { mutableStateOf(lang.read.value?.chosen ?: "") }
            val rows = lang.read.value?.langs.orEmpty()
            Line("Which language your captures are read and written in — artifact text, titles, tags. It applies to what you capture from now on.", muted = true)
            Row(Modifier.fillMaxWidth().clickable { choosing = true }.padding(vertical = 6.dp), horizontalArrangement = Arrangement.SpaceBetween) {
                Line(rows.firstOrNull { it.value == chosen }?.label ?: "Automatic — follow this phone")
                Line("change", muted = true)
            }
            if (choosing) AlertDialog(
                onDismissRequest = { choosing = false },
                title = { Text("Capture language") },
                text = {
                    Column(Modifier.verticalScroll(rememberScrollState())) {
                        (listOf("" to "Automatic — follow this phone") + rows.map { it.value to it.label }).forEach { (v, l) ->
                            Row(Modifier.fillMaxWidth().clickable {
                                choosing = false
                                scope.launch {
                                    val r = engram.reader.call("PUT", Api.LANG, Api.json("lang" to v), Decode.nothing)
                                    if (r.value != null) chosen = v
                                }
                            }, verticalAlignment = Alignment.CenterVertically) {
                                RadioButton(selected = v == chosen, onClick = null)
                                Text(l, style = MaterialTheme.typography.bodyMedium)
                            }
                        }
                    }
                },
                confirmButton = { TextButton(onClick = { choosing = false }) { Text("Close") } },
            )
        }
        // The channels a due reminder is pushed to. The phone's own
        // registration is the UnifiedPush row; the fields are the web's, for
        // Gotify or an endpoint pasted by hand.
        Section("Notifications") {
            val notify = rememberRead(engram, Api.notify(), Decode.notify)
            val n = notify.read.value
            var url by remember(n?.gotifyUrl) { mutableStateOf(n?.gotifyUrl ?: "") }
            var token by remember { mutableStateOf("") }
            var endpoint by remember(n?.upEndpoint) { mutableStateOf(n?.upEndpoint ?: "") }
            var result by remember { mutableStateOf<String?>(null) }
            // Nothing may be typed or saved before the answer is in. Save
            // writes all three fields, and an empty field switches a channel
            // off — so a Save pressed while the read was still in flight would
            // send the empty defaults and delete this phone's own UnifiedPush
            // registration, endpoint and keys, along with any Gotify channel.
            val known = n != null
            Waiting(notify)
            Line("A due reminder is pushed here. Leave a field empty to switch that channel off.", muted = true)
            OutlinedTextField(value = url, onValueChange = { url = it }, enabled = known, label = { Text("Gotify message URL") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            OutlinedTextField(value = token, onValueChange = { token = it }, enabled = known, label = { Text(if (n?.gotifyTokenSet == true) "Gotify app token · kept unless you type a new one" else "Gotify app token") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            OutlinedTextField(value = endpoint, onValueChange = { endpoint = it }, enabled = known, label = { Text("UnifiedPush endpoint") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            when {
                n?.upLegacy == true -> Line("Legacy endpoint · plaintext. Pair the app to encrypt.", muted = true)
                n?.upDevice != null -> Line("Registered by ${n.upDevice} · encrypted", muted = true)
            }
            Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                Button(enabled = known, onClick = {
                    scope.launch {
                        val r = engram.reader.call("PUT", Api.NOTIFY, Api.json("gotify_url" to url, "gotify_token" to token, "up_endpoint" to endpoint), Decode.nothing)
                        result = if (r.value != null) "Saved." else r.error ?: "Server unreachable"
                        if (r.value != null) { token = ""; notify.retry() }
                    }
                }) { Text("Save") }
                for ((label, channel) in listOf("Test Gotify" to "gotify", "Test UnifiedPush" to "unifiedpush")) {
                    TextButton(onClick = {
                        scope.launch {
                            val r = engram.reader.call("POST", Api.NOTIFY_TEST, Api.json("channel" to channel), Decode.notifyTested)
                            result = r.value?.let { if (it.sent) "Sent." else it.error } ?: r.error ?: "Server unreachable"
                        }
                    }) { Text(label) }
                }
            }
            result?.let { Line(it, muted = true) }
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
        // What this installation is keeping about how it is used, and the one
        // button that destroys it.
        Section("What is being recorded") {
            val feedback = rememberRead(engram, Api.feedback(), Decode.feedback)
            var purging by remember { mutableStateOf(false) }
            var purged by remember { mutableStateOf<String?>(null) }
            val f = feedback.read.value
            val fs = f?.searches
            when {
                f == null -> {}
                fs == null -> Line("Not recording searches.", muted = true)
                else -> {
                    Line("Searches — ${fs.captured} captured, ${fs.pending} waiting" + (f.asks?.let { "; questions — ${it.asked} asked, ${it.judged} judged" } ?: "") + ".", muted = true)
                    purged?.let { Line(it, muted = true) }
                    OutlinedButton(onClick = { purging = true }, colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error)) {
                        Text("Delete all captured searches, questions and pursuits")
                    }
                }
            }
            if (purging) AlertDialog(
                onDismissRequest = { purging = false },
                title = { Text("Delete every recorded search and question?") },
                text = { Text("And every verdict given on one. The judged ones cannot be recovered — they are what the retrieval figure is measured from.") },
                confirmButton = {
                    TextButton(onClick = {
                        purging = false
                        scope.launch {
                            val r = engram.reader.call("DELETE", Api.FEEDBACK, null, Decode.nothing)
                            purged = if (r.value != null) "Deleted." else r.error ?: "Server unreachable"
                            feedback.retry()
                        }
                    }, colors = ButtonDefaults.textButtonColors(contentColor = MaterialTheme.colorScheme.error)) { Text("Delete") }
                },
                dismissButton = { TextButton(onClick = { purging = false }) { Text("Keep") } },
            )
        }
        // The web's toggle. Kept on the phone: nothing about it is the base's.
        Section("Theme") {
            val theme by engram.theme.collectAsStateWithLifecycle()
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                for (m in ThemeMode.entries) {
                    FilterChip(selected = theme.equals(m.name, ignoreCase = true), onClick = { engram.setTheme(m.name.lowercase()) }, label = { Text(m.name) })
                }
            }
        }
        Section("Insights") {
            JudgeLine("What this memory is like", null) { onJudging(Screen.Insights) }
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
        // The way to the queue while nothing is owed. It leaves the top bar
        // then; this is where somebody who wants to look anyway can.
        Section("Queue") {
            val owed by engram.outbox.rows.collectAsStateWithLifecycle(emptyList())
            val n = queuedCount(owed)
            JudgeLine(if (n > 0) "$n owed to the server" else "Nothing owed", null, onQueue)
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
