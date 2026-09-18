package io.github.overcuriousity.engram.ui

import android.Manifest
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.DueRow
import io.github.overcuriousity.engram.core.read.Offer
import io.github.overcuriousity.engram.core.sync.Sync
import io.github.overcuriousity.engram.doors.Intake
import io.github.overcuriousity.engram.doors.Microphone
import kotlinx.coroutines.launch
import java.io.File
import java.time.ZoneId

/**
 * Home, and the only surface there is: one box that searches while it is typed
 * into and keeps what is put in it, exactly as the web and the PWA have it.
 * Beneath an empty box, what the base has to say unasked — the offer for this
 * situation, what is due, what is worth seeing again.
 *
 * There used to be a second box on a Capture tab. Two boxes is one question
 * the person has to answer before they can type: which of these did I mean.
 * The verb is a button under the one box, and never a guess made from the text.
 *
 * There also used to be a Record verb that kept a voice note as a file for the
 * queue to send. That was not what the web's microphone does. Its button is
 * held, and what is said is typed into the box — dictation, not capture — and
 * the same press still decides what the words are for. `Microphone` is that.
 */
@Composable
fun SearchScreen(
    engram: Engram,
    onArtifact: (String) -> Unit,
    onAsk: (String) -> Unit,
    focusBox: Boolean = false,
) {
    val scope = rememberCoroutineScope()
    val ctx = LocalContext.current
    var text by rememberSaveable { mutableStateOf("") }
    var title by rememberSaveable { mutableStateOf("") }
    var note by rememberSaveable { mutableStateOf("") }
    var files by remember { mutableStateOf(listOf<Uri>()) }
    var queued by remember { mutableStateOf<String?>(null) }

    // The microphone: held open while the button is, then the words come
    // back into the box. Drawn only where the server says it has a speech
    // model — the web draws its button on the same fact — and drawn while
    // that is not yet known, because a door that may be open is worth a press.
    val mic = remember { Microphone(ctx) }
    var micState by remember { mutableStateOf(MicState()) }
    val status = rememberRead(engram, Api.status(), Decode.status)
    val micOpen = status.read.value?.transcribe ?: true

    // What the box has asked for. A keystroke does not ask; typing that has
    // stood still for a moment does. See `Typing.kt`.
    var asked by rememberSaveable { mutableStateOf("") }
    LaunchedEffect(Unit) { snapshotFlow { text }.queries().collect { asked = it } }

    val pick = rememberLauncherForActivityResult(ActivityResultContracts.GetMultipleContents()) { files = files + it }
    // TakePicture writes the full-resolution image the camera actually took.
    // TakePicturePreview, which this used, hands back the shutter thumbnail —
    // a photo of a page or a whiteboard came through too small to read or OCR.
    var pending by remember { mutableStateOf<File?>(null) }
    val photo = rememberLauncherForActivityResult(ActivityResultContracts.TakePicture()) { ok ->
        val f = pending
        pending = null
        if (ok && f != null && f.length() > 0) files = files + Uri.fromFile(f) else f?.delete()
    }
    fun shoot() {
        val f = File.createTempFile("photo", ".jpg", engram.app.cacheDir)
        pending = f
        photo.launch(FileProvider.getUriForFile(ctx, "${ctx.packageName}.files", f))
    }
    // An app that declares CAMERA must hold it before the camera app will
    // answer ACTION_IMAGE_CAPTURE for it, the same as the microphone door.
    val camera = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { ok ->
        if (ok) shoot()
    }
    val micPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { ok ->
        // The press that asked is spent, as it is on the web where the
        // browser's prompt eats the first hold. The next one records.
        micState = MicState(said = if (ok) "" else "No microphone — permission refused.")
    }
    fun micDown() {
        if (micState.busy) return
        if (!mic.allowed) { micPermission.launch(Manifest.permission.RECORD_AUDIO); return }
        micState = if (mic.start()) MicState(listening = true, said = "Listening…") else MicState(said = "No microphone.")
    }
    fun micUp() {
        if (!micState.listening) return
        val heard = mic.stop()
        // A press and a release with nothing in between: nothing happened,
        // and saying so is noise.
        if (heard.size <= Microphone.HEADER) { micState = MicState(); return }
        micState = MicState(busy = true, said = "Transcribing…")
        scope.launch {
            val r = engram.reader.hear(heard, Microphone.MIME)
            val words = r.value?.trim().orEmpty()
            micState = MicState(said = if (r.value != null) "" else r.error ?: "Could not transcribe that.")
            // At the end, never over what is there: the box may hold a
            // question half typed, and a microphone is not a reason to lose
            // it. The box that changes is the box that searches — dictation
            // fills it, and what happens next is still a press.
            if (words.isNotEmpty()) text = text.trimEnd().let { if (it.isEmpty()) words else "$it $words" }
        }
    }

    fun capture() {
        val t = text.trim()
        val ti = title.trim().ifEmpty { null }
        val n = note.trim().ifEmpty { null }
        val held = files
        // The box is cleared here rather than when the intake returns, so the
        // surface is ready for the next thing at once and a slow queue write
        // cannot hand back a box that has been typed into since.
        text = ""; title = ""; note = ""; files = emptyList(); asked = ""
        scope.launch {
            queued = if (held.isNotEmpty()) Intake.uris(engram, held, ti, n ?: t.ifEmpty { null })
            else Intake.text(engram, t, ti, n)
        }
    }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        HomeBox(
            text = text,
            onText = { text = it; if (it.isBlank()) { asked = ""; queued = null } },
            files = files,
            onDrop = { files = files - it },
            title = title, onTitle = { title = it },
            note = note, onNote = { note = it },
            mic = if (micOpen) micState else null,
            focus = focusBox,
            onAttach = { pick.launch("*/*") },
            onPhoto = { camera.launch(Manifest.permission.CAMERA) },
            onMicDown = ::micDown,
            onMicUp = ::micUp,
            onAsk = { onAsk(text.trim()) },
            onCapture = ::capture,
        )
        // What became of the last capture, from the row itself: kept, still
        // on its way, or refused. It used to say "Queued · see Queue" for
        // every capture, which sent a person to a screen to find out that
        // nothing was wrong.
        val id = queued
        if (id != null && text.isBlank()) {
            val rows by engram.outbox.rows.collectAsStateWithLifecycle(emptyList())
            val words = captureWords(rows.firstOrNull { it.id == id })
            Text(
                words.text,
                Modifier.padding(16.dp, 4.dp),
                style = MaterialTheme.typography.bodySmall,
                color = if (words.wrong) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.tertiary,
            )
        }
        if (asked.isEmpty()) Idle(engram, onArtifact) else Results(engram, asked, onArtifact)
    }
}

/**
 * The box and the verbs under it. Nothing here knows about a server, which is
 * what lets the whole surface be drawn in a test.
 *
 * The verb is always a press. A box that decided between searching, asking and
 * keeping by reading what was typed into it would be wrong on the day somebody
 * keeps a question, and there is no telling them it guessed.
 */
@Composable
fun HomeBox(
    text: String,
    onText: (String) -> Unit,
    files: List<Uri> = emptyList(),
    onDrop: (Uri) -> Unit = {},
    title: String = "",
    onTitle: (String) -> Unit = {},
    note: String = "",
    onNote: (String) -> Unit = {},
    /** The microphone, or null where the server has no speech model and there is no button to hold. */
    mic: MicState? = MicState(),
    focus: Boolean = false,
    onAttach: () -> Unit = {},
    onPhoto: () -> Unit = {},
    onMicDown: () -> Unit = {},
    onMicUp: () -> Unit = {},
    onAsk: () -> Unit = {},
    onCapture: () -> Unit = {},
) {
    var more by remember { mutableStateOf(false) }
    val requester = remember { FocusRequester() }
    // The tile and the launcher shortcut are a capture in one press; they open
    // here, and the box they open is the one already waiting for the text.
    LaunchedEffect(focus) { if (focus) runCatching { requester.requestFocus() } }

    Column(Modifier.padding(16.dp, 8.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        OutlinedTextField(
            value = text,
            onValueChange = onText,
            modifier = Modifier.fillMaxWidth().focusRequester(requester),
            placeholder = { Text("Ask, search, or paste to keep…") },
            // A box from the first keystroke to the last: it grows to a
            // ten-line cap and then scrolls inside itself, as the web's does,
            // and it never changes shape under what is being written.
            minLines = 3,
            maxLines = 10,
        )
        if (files.isNotEmpty()) LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            items(files) { u -> AssistChip(onClick = { onDrop(u) }, label = { Text(u.lastPathSegment ?: "file", maxLines = 1) }) }
        }
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = onAttach) { Text("Attach") }
            TextButton(onClick = onPhoto) { Text("Photo") }
            TextButton(onClick = { more = !more }) { Text(if (more) "Less" else "Title · note") }
            Spacer(Modifier.weight(1f))
            // Held, not pressed: the web's button and every messenger's.
            if (mic != null) MicButton(mic, onDown = onMicDown, onUp = onMicUp)
        }
        if (mic != null && mic.said.isNotEmpty()) {
            Text(mic.said, style = MaterialTheme.typography.bodySmall, color = muted())
        }
        if (more) {
            OutlinedTextField(value = title, onValueChange = onTitle, label = { Text("Title") }, singleLine = true, modifier = Modifier.fillMaxWidth())
            OutlinedTextField(value = note, onValueChange = onNote, label = { Text("Note") }, modifier = Modifier.fillMaxWidth())
        }
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End)) {
            OutlinedButton(onClick = onAsk, enabled = text.isNotBlank()) { Text("Ask") }
            Button(onClick = onCapture, enabled = text.isNotBlank() || files.isNotEmpty()) { Text("Capture") }
        }
    }
}

/**
 * The answer, and the answer before it while the next one is on its way. A
 * read keyed on the query starts empty, so without holding the last one the
 * rail blanked between a word and the word after it — on a box that asks on
 * every settled keystroke, that is most of the time you are looking at it.
 */
@Composable
private fun Results(engram: Engram, q: String, onArtifact: (String) -> Unit) {
    val state = rememberRead(engram, Api.search(q), Decode.hits)
    val page = held(state)
    Column {
        Waiting(state)
        page?.let {
            if (it.items.isEmpty()) Text("Nothing", Modifier.padding(16.dp), color = muted())
            Rail(railOf(it.items), onArtifact)
        }
    }
}

@Composable
private fun Idle(engram: Engram, onArtifact: (String) -> Unit) {
    OfferCard(engram, onArtifact)
    DueList(engram, onArtifact)
    val again = rememberRead(engram, Api.resurface(), Decode.hits)
    ReadFrame(again) { page ->
        if (page.items.isNotEmpty()) {
            SectionHead("Worth seeing again")
            // Not a ranked list and not an answer to anything: no ranks, no rule.
            page.items.forEach { h ->
                val (name, named) = nameOf(h)
                LinkRow(name, named) { onArtifact(h.artifactId) }
            }
        }
    }
}

/**
 * The offer for the situation the phone is in. Asked once per visit to home and
 * never kept: it belongs to this moment. `seen` is posted from inside the card,
 * so it runs when the card is composed — on screen — and not when the answer
 * arrives: an offer that lost the race to a typed query was never shown, and
 * counting it would put a population that cannot tap into the hit rate.
 */
@Composable
private fun OfferCard(engram: Engram, onArtifact: (String) -> Unit) {
    var offer by remember { mutableStateOf<Offer?>(null) }
    LaunchedEffect(Unit) {
        offer = engram.reader.ask(Api.CONTEXT, engram.situation.bundle(engram.placeOn).toString(), Decode.offer).value?.offer
    }
    val o = offer ?: return
    LaunchedEffect(o.artifactId, o.rung) { engram.reader.tell(Api.SEEN, Api.seen(o)) }
    Surface(
        color = MaterialTheme.colorScheme.surface,
        shape = MaterialTheme.shapes.medium,
        modifier = Modifier.fillMaxWidth().padding(16.dp, 12.dp).clickable { onArtifact(o.artifactId) },
    ) {
        Column(Modifier.padding(14.dp)) {
            val why = offerLine(o, ZoneId.systemDefault())
            Text(if (why.isEmpty()) "Offered" else "Offered · $why", style = MaterialTheme.typography.labelSmall, color = muted())
            Label(o.label, o.named, Modifier.padding(top = 4.dp))
            if (o.named && o.snippet.isNotEmpty()) {
                Text(o.snippet, Modifier.padding(top = 2.dp), maxLines = 3, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
    }
}

/** What is due, each row with the two actions its notification has. */
@Composable
fun DueList(engram: Engram, onArtifact: (String) -> Unit) {
    val due = rememberRead(engram, Api.due(), Decode.due)
    ReadFrame(due) { page ->
        if (page.items.isNotEmpty()) {
            SectionHead("Due")
            page.items.forEach { DueLine(engram, it, onArtifact) }
        }
    }
}

/**
 * Done and snooze are writes, and every write the device owes goes through the
 * outbox: the row is settled on screen at once and delivered when the server
 * can be reached, exactly as the notification's two buttons are.
 */
@Composable
fun DueLine(engram: Engram, row: DueRow, onArtifact: (String) -> Unit) {
    val scope = rememberCoroutineScope()
    val settled = remember { mutableStateListOf<String>() }
    val isSettled = row.moment.id in settled
    fun settle(write: suspend () -> Unit) {
        settled += row.moment.id
        scope.launch { write(); Sync.kick(engram.app) }
    }
    Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f).clickable { onArtifact(row.moment.artifactId) }.padding(vertical = 8.dp)) {
            Text(
                row.title, maxLines = 1,
                style = MaterialTheme.typography.bodyLarge,
                textDecoration = if (isSettled) TextDecoration.LineThrough else null,
                color = if (row.named && !isSettled) MaterialTheme.colorScheme.onBackground else MaterialTheme.colorScheme.onSurfaceVariant,
            )
            val words = dueWords(row.at, System.currentTimeMillis())
            if (words.isNotEmpty()) Text(words, style = MaterialTheme.typography.labelSmall, color = due())
        }
        if (!isSettled) {
            TextButton(onClick = { settle { engram.outbox.enqueueSnooze(row.moment.id, System.currentTimeMillis() / 1000 + 3600) } }) { Text("1 h") }
            TextButton(onClick = { settle { engram.outbox.enqueueDone(row.moment.id) } }) { Text("Done") }
        }
    }
}
