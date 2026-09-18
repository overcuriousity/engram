package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.FilterChip
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.core.AskVia
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.Mode
import io.github.overcuriousity.engram.core.contained.Core
import io.github.overcuriousity.engram.core.contained.Endpoint
import io.github.overcuriousity.engram.core.contained.ModelManifest
import io.github.overcuriousity.engram.core.contained.Role
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import kotlinx.coroutines.launch

/** What waits for a model, and what it waits for. One line, and only in Settings: no banner, no badge. */
fun backgroundWords(waiting: Int, passWanted: Boolean): String = when {
    waiting <= 0 -> "Background · nothing waiting"
    !passWanted -> "Background · $waiting waiting · no endpoint"
    else -> "Background · $waiting waiting · charging and idle"
}

/**
 * Where this app's engram lives, and the switch. What was the "Server"
 * section: in server mode it still says everything that section said.
 */
@Composable
fun ModeSection(engram: Engram) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val paired by engram.store.current.collectAsStateWithLifecycle()
    var asking by remember { mutableStateOf<Mode?>(null) }
    SettingsSection("Mode") {
        if (engram.mode == Mode.contained) {
            SettingsLine("On this phone")
            OutlinedButton(onClick = { asking = Mode.server }) { Text("Switch to a server") }
            // The one place that says what model work is waiting, and why.
            val status = rememberRead(engram, Api.status(), Decode.status)
            status.read.value?.let { SettingsLine(backgroundWords(it.waitingGeneration, engram.passWanted), muted = true) }
        } else {
            SettingsLine(paired?.origin ?: "—")
            SettingsLine("version ${paired?.serverVersion ?: "—"}", muted = true)
            SettingsLine(paired?.deviceName ?: "", muted = true)
            SettingsLine(if (paired?.pin != null) "pinned · ${paired!!.pin!!.take(12)}…" else "public certificate · no pin", muted = true)
            if (Core.available) OutlinedButton(onClick = { asking = Mode.contained }) { Text("Switch to this phone") }
        }
    }
    asking?.let { to ->
        SwitchDialog(to, onSwitch = { asking = null; scope.launch { Engram.switch(ctx, to) } }, onStay = { asking = null })
    }
}

/** What is on the phone and what could be. The speech model is not here until the phone can use one. */
@Composable
fun ModelsSection(engram: Engram) {
    val scope = rememberCoroutineScope()
    SettingsSection("Models") {
        ModelManifest.all.filter { it.role == Role.embed || it.role == Role.ask }.forEach { m ->
            ModelLine(engram, m, onChanged = { scope.launch { engram.restartCore() } })
        }
    }
}

/** How a contained phone answers questions: its own model, somebody's endpoint, or not at all. */
@Composable
fun AskSection(engram: Engram) {
    val scope = rememberCoroutineScope()
    var via by remember { mutableStateOf(engram.modes.ask) }
    val held = remember { engram.askEndpoint }
    var url by remember { mutableStateOf(held?.baseUrl ?: "") }
    var model by remember { mutableStateOf(held?.model ?: "") }
    var key by remember { mutableStateOf(held?.apiKey ?: "") }
    var saved by remember { mutableStateOf(false) }
    fun set(v: AskVia) {
        via = v
        engram.modes.ask = v
        // An endpoint that was never filled in is set by Save, not by the chip.
        if (v != AskVia.endpoint || engram.askEndpoint != null) scope.launch { engram.restartCore() }
    }
    SettingsSection("Ask") {
        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            FilterChip(selected = via == AskVia.device, onClick = { set(AskVia.device) }, label = { Text("On this phone") })
            FilterChip(selected = via == AskVia.endpoint, onClick = { set(AskVia.endpoint) }, label = { Text("Endpoint") })
            FilterChip(selected = via == AskVia.off, onClick = { set(AskVia.off) }, label = { Text("Off") })
        }
        if (via == AskVia.endpoint) {
            OutlinedTextField(value = url, onValueChange = { url = it; saved = false }, label = { Text("Base URL") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            OutlinedTextField(value = model, onValueChange = { model = it; saved = false }, label = { Text("Model") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            OutlinedTextField(value = key, onValueChange = { key = it; saved = false }, label = { Text("API key") }, singleLine = true, visualTransformation = PasswordVisualTransformation(), modifier = Modifier.fillMaxWidth())
            Button(enabled = url.isNotBlank() && model.isNotBlank(), onClick = {
                engram.askEndpoint = Endpoint(url.trim(), model.trim(), key.trim().ifEmpty { null })
                saved = true
                scope.launch { engram.restartCore() }
            }) { Text("Save") }
            if (saved) SettingsLine("Saved.", muted = true)
        }
    }
}
