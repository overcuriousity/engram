package io.github.overcuriousity.engram.ui

import android.Manifest
import android.graphics.Bitmap
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.doors.AudioNote
import io.github.overcuriousity.engram.doors.Intake
import kotlinx.coroutines.launch
import java.io.File

@Composable
fun ComposeScreen(engram: Engram, justQueued: String? = null) {
    val scope = rememberCoroutineScope()
    val ctx = LocalContext.current
    var text by remember { mutableStateOf("") }
    var title by remember { mutableStateOf("") }
    var note by remember { mutableStateOf("") }
    var more by remember { mutableStateOf(false) }
    var files by remember { mutableStateOf(listOf<Uri>()) }
    var confirmed by remember { mutableStateOf(justQueued) }
    val audio = remember { AudioNote(ctx) }
    var recording by remember { mutableStateOf(false) }

    val pick = rememberLauncherForActivityResult(ActivityResultContracts.GetMultipleContents()) { files = files + it }
    val photo = rememberLauncherForActivityResult(ActivityResultContracts.TakePicturePreview()) { bmp ->
        bmp ?: return@rememberLauncherForActivityResult
        val f = File.createTempFile("photo", ".jpg", engram.app.cacheDir)
        f.outputStream().use { bmp.compress(Bitmap.CompressFormat.JPEG, 90, it) }
        files = files + Uri.fromFile(f)
    }
    val mic = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { ok ->
        if (ok) { audio.start(); recording = true }
    }

    fun send() {
        val t = text.trim()
        val ti = title.trim().ifEmpty { null }
        val n = note.trim().ifEmpty { null }
        scope.launch {
            val id = if (files.isNotEmpty()) Intake.uris(engram, files, ti, n ?: t.ifEmpty { null })
            else Intake.text(engram, t, ti, n)
            text = ""; title = ""; note = ""; files = emptyList(); confirmed = id
        }
    }

    Column(Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        confirmed?.let {
            Text("Queued · see Queue", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.tertiary)
        }
        OutlinedTextField(
            value = text, onValueChange = { text = it }, modifier = Modifier.fillMaxWidth().weight(1f),
            placeholder = { Text("Paste something to keep…") },
        )
        if (files.isNotEmpty()) LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            items(files) { u -> AssistChip(onClick = { files = files - u }, label = { Text(u.lastPathSegment ?: "file", maxLines = 1) }) }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            TextButton(onClick = { pick.launch("*/*") }) { Text("Attach") }
            TextButton(onClick = { photo.launch(null) }) { Text("Photo") }
            TextButton(onClick = {
                if (recording) {
                    audio.stop()?.let { files = files + Uri.fromFile(it) }
                    recording = false
                } else {
                    mic.launch(Manifest.permission.RECORD_AUDIO)
                }
            }) { Text(if (recording) "Stop" else "Record") }
            TextButton(onClick = { more = !more }) { Text(if (more) "Less" else "Title · note") }
        }
        if (more) {
            OutlinedTextField(value = title, onValueChange = { title = it }, label = { Text("Title") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            OutlinedTextField(value = note, onValueChange = { note = it }, label = { Text("Note") }, modifier = Modifier.fillMaxWidth())
        }
        Button(onClick = ::send, enabled = !recording && (text.isNotBlank() || files.isNotEmpty()), modifier = Modifier.fillMaxWidth()) { Text("Keep") }
    }
}
