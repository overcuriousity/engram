package io.github.overcuriousity.engram.ui

import android.Manifest
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
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
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
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
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.contained.ModelManifest
import io.github.overcuriousity.engram.core.contained.Role
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.Offer
import io.github.overcuriousity.engram.core.read.SearchPage
import io.github.overcuriousity.engram.core.read.Status
import io.github.overcuriousity.engram.doors.Intake
import io.github.overcuriousity.engram.doors.Microphone
import kotlinx.coroutines.launch
import java.io.File
import java.time.ZoneId

/**
 * Home, and the only surface there is: one box that searches while it is typed
 * into and keeps what is put in it, exactly as the web and the PWA have it.
 * Beneath an empty box, what the base has to say unasked — the offer for this
 * situation, what is due, what it holds and what a paste becomes.
 *
 * There used to be a second box on a Capture tab. Two boxes is one question
 * the person has to answer before they can type: which of these did I mean.
 * The verb is a button under the one box, and never a guess made from the text.
 *
 * There also used to be a Record verb that kept a voice note as a file for the
 * queue to send. That was not what the web's microphone does. Its button is
 * held, and what is said is typed into the box — dictation, not capture — and
 * the same press still decides what the words are for. `Microphone` is that.
 *
 * [prefill] and [fromAsk] are the *edit first* door: an answer engram wrote,
 * put in the box to be rewritten, with the question it came from riding the
 * capture so what gets stored says where it came from.
 */
@Composable
fun SearchScreen(
    engram: Engram,
    onArtifact: (String) -> Unit,
    onHit: (String, String?) -> Unit,
    onAsk: (String) -> Unit,
    onCorpus: (String) -> Unit,
    focusBox: Boolean = false,
    prefill: String = "",
    fromAsk: String? = null,
    question: String = "",
) {
    val scope = rememberCoroutineScope()
    val ctx = LocalContext.current
    var text by rememberSaveable { mutableStateOf(prefill) }
    var title by rememberSaveable { mutableStateOf("") }
    var note by rememberSaveable { mutableStateOf("") }
    var category by rememberSaveable { mutableStateOf("") }
    var files by remember { mutableStateOf(listOf<Uri>()) }
    var queued by remember { mutableStateOf<String?>(null) }
    // The claim about *this* text. Taken away with the capture, so the next
    // thing pasted into the same box is not stored as the same model answer.
    var keptFrom by rememberSaveable { mutableStateOf(fromAsk) }

    // Which doors are open, and what the idle column says. Read through the
    // cache, so the second visit knows before the server answers.
    val status = rememberRead(engram, Api.status(), Decode.status)
    val doors = status.read.value

    // The microphone: held open while the button is, then the words come
    // back into the box. Drawn only where the server says it has a speech
    // model — the web draws its button on the same fact — and drawn while
    // that is not yet known, because a door that may be open is worth a press.
    val mic = remember { Microphone(ctx) }
    var micState by remember { mutableStateOf(MicState()) }
    // On a phone that is its own engram the button is there before the model
    // is: the first press is where the model is offered.
    var speechLooked by remember { mutableIntStateOf(0) }
    val speechWanted = remember(speechLooked) { engram.speechWanted }
    var speechOffer by remember { mutableStateOf(false) }
    val micOpen = (doors?.transcribe ?: true) || speechWanted
    // A hold that never gets its release: the screen can leave composition
    // mid-press — a rotation is enough — and `tryAwaitRelease` is cancelled
    // with it, so nothing would ever close the door. The recorder and its
    // thread would then hold the microphone for as long as the process lives.
    DisposableEffect(mic) { onDispose { mic.stop() } }

    // What the box has asked for. A keystroke does not ask; typing that has
    // stood still for a moment does. See `Typing.kt`. A box filled by a door
    // is filled and still: what was put there is not a question.
    var asked by rememberSaveable { mutableStateOf("") }
    LaunchedEffect(Unit) { snapshotFlow { text }.queries().collect { if (it != prefill || prefill.isEmpty()) asked = it } }

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
        if (speechWanted) { speechOffer = true; return }
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
        val from = keptFrom
        // The box is cleared here rather than when the intake returns, so the
        // surface is ready for the next thing at once and a slow queue write
        // cannot hand back a box that has been typed into since.
        text = ""; title = ""; note = ""; files = emptyList(); asked = ""; keptFrom = null
        scope.launch {
            queued = if (held.isNotEmpty()) Intake.uris(engram, held, ti, n ?: t.ifEmpty { null })
            else Intake.text(engram, t, ti, n, from)
        }
    }

    if (speechOffer) SpeechOfferDialog(
        model = {
            ModelManifest.defaultFor(Role.speech)?.let { m ->
                // Arrived: a new core over it, and the status read again, which
                // is what turns the offer's button into the microphone.
                ModelLine(engram, m, onChanged = { scope.launch { engram.restartCore(); speechLooked++; status.retry() } })
            }
        },
        onNotNow = { engram.modes.speechDeclined = true; speechOffer = false; speechLooked++ },
        onDismiss = { speechOffer = false; speechLooked++ },
    )
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        if (keptFrom != null) {
            Text(
                "Kept from: “$question” — an answer engram wrote. Edit it as you like; saving is yours, and the capture records the question it came from.",
                Modifier.padding(16.dp, 8.dp), style = MaterialTheme.typography.bodySmall, color = muted(),
            )
        }
        HomeBox(
            text = text,
            onText = { text = it; if (it.isBlank()) { asked = ""; queued = null } },
            files = files,
            onDrop = { files = files - it },
            title = title, onTitle = { title = it },
            note = note, onNote = { note = it },
            mic = if (micOpen) micState else null,
            focus = focusBox,
            // Ask is a door the server opens; a staged file disarms it, as it
            // does on the web: the box is that file's note by then.
            askOpen = (doors?.asks ?: true) && files.isEmpty(),
            onAttach = { pick.launch("*/*") },
            onPhoto = { camera.launch(Manifest.permission.CAMERA) },
            onMicDown = ::micDown,
            onMicUp = ::micUp,
            onAsk = { onAsk(text.trim()) },
            onCapture = ::capture,
        )
        // The one line under the box: an example teaches what typing does
        // better than a claim did. A chip fills the box and does not submit.
        BoxHint(doors, onExample = { text = it })
        // What capture will do with the box, said before it is pressed.
        if (text.isNotBlank() && files.isEmpty()) IntentEcho(engram, asked)
        // Chips qualify a search. On an idle page there is no search.
        if (text.isNotBlank()) KindChips(engram, category) { category = it }
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
        if (asked.isEmpty()) Idle(engram, doors, onArtifact, onCorpus) else Results(engram, asked, category, doors, onHit)
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
    /** Whether Ask is a door here at all: the server has an ask model, and nothing is staged. */
    askOpen: Boolean = true,
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
            if (askOpen) OutlinedButton(onClick = onAsk, enabled = text.isNotBlank()) { Text("Ask") }
            Button(onClick = onCapture, enabled = text.isNotBlank() || files.isNotEmpty()) { Text("Capture") }
        }
    }
}

/**
 * The web's `_box_hint`: one sentence, and the classifier's own two example
 * phrasings in the reader's language. Pressing one puts the phrasing in front
 * of you; a press is still yours to make.
 */
@Composable
fun BoxHint(doors: Status?, onExample: (String) -> Unit) {
    val held = (doors?.held?.corpora ?: 1) > 0
    val ex = doors?.examples
    Column(Modifier.padding(16.dp, 0.dp)) {
        Text(
            if (held) "A whole sentence finds more than keywords do." else "Paste anything worth keeping — a note, an article, a chunk of a chat. engram finds it again by meaning, and nobody else can search this base.",
            style = MaterialTheme.typography.bodySmall, color = muted(),
        )
        if (ex != null && ex.remind.isNotEmpty()) {
            FlowRow(verticalArrangement = Arrangement.spacedBy(0.dp), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                Text("Try", Modifier.padding(top = 10.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                TextButton(onClick = { onExample(ex.remind) }) { Text("“${ex.remind}”", style = MaterialTheme.typography.bodySmall) }
                Text("or", Modifier.padding(top = 10.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                TextButton(onClick = { onExample(ex.journal) }) { Text("“${ex.journal}”", style = MaterialTheme.typography.bodySmall) }
            }
        }
    }
}

/** What capture will do with the box: synthesized, or stored verbatim window by window. Counted by the server, no model call. */
@Composable
private fun IntentEcho(engram: Engram, asked: String) {
    if (asked.isEmpty()) return
    val echo = rememberRead(engram, Api.echo(asked), Decode.echo)
    val e = held(echo) ?: return
    if (e.kind.isEmpty()) return
    Row(Modifier.padding(16.dp, 2.dp)) {
        Text(e.kind, style = MaterialTheme.typography.labelSmall, fontWeight = FontWeight.Medium, color = MaterialTheme.colorScheme.primary)
        Text(" · ${e.detail}", style = MaterialTheme.typography.labelSmall, color = muted())
    }
}

/** One row, one taxonomy: the kinds the base holds, qualifying what typing does. */
@Composable
private fun KindChips(engram: Engram, category: String, onCategory: (String) -> Unit) {
    val facets = rememberRead(engram, Api.facets(), Decode.facets)
    val kinds = held(facets)?.categories.orEmpty()
    if (kinds.isEmpty()) return
    Row(Modifier.padding(16.dp, 0.dp), verticalAlignment = Alignment.CenterVertically) {
        Text("Kind", Modifier.padding(end = 8.dp), style = MaterialTheme.typography.labelSmall, color = muted())
        LazyRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            item { FilterChip(selected = category.isEmpty(), onClick = { onCategory("") }, label = { Text("All") }) }
            items(kinds) { k -> FilterChip(selected = category == k.value, onClick = { onCategory(k.value) }, label = { Text(k.value) }) }
        }
    }
}

/**
 * The answer, and the answer before it while the next one is on its way. A
 * read keyed on the query starts empty, so without holding the last one the
 * rail blanked between a word and the word after it — on a box that asks on
 * every settled keystroke, that is most of the time you are looking at it.
 *
 * Two passes, as the web makes them: the typing pass at vector-order speed,
 * then — once it has landed — the refining pass, reranked and explained, which
 * replaces it and marks the list *refined*. The event either was recorded
 * under is what an open from this list names.
 */
@Composable
private fun Results(engram: Engram, q: String, category: String, doors: Status?, onHit: (String, String?) -> Unit) {
    val typing = rememberRead(engram, Api.search(q, category), Decode.search)
    val first = held(typing)
    var refined: SearchPage? = null
    if (typing.read.value != null) {
        val pass = rememberRead(engram, Api.search(q, category, refine = true), Decode.search)
        refined = pass.read.value
    }
    val page = refined ?: first
    Column {
        Waiting(typing)
        page?.let { p ->
            val rail = railOf(p.items)
            val loose = p.items.count { it.weak }
            val allLoose = rail.any { it == RailItem.NothingClose }
            // Bound here rather than read twice: it crosses a module boundary, so it does not smart-cast.
            val event = p.event
            // Counted off the list rather than passed in beside it, so the
            // number and the rows cannot disagree.
            Row(Modifier.padding(16.dp, 6.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    "${p.items.size} result${if (p.items.size != 1) "s" else ""}" +
                        (if (loose > 0 && !allLoose) " · $loose loose" else "") +
                        (if (p.reranked) " · refined" else ""),
                    style = MaterialTheme.typography.labelSmall, color = muted(),
                )
            }
            if (p.items.isEmpty()) Text("No matches.", Modifier.padding(16.dp, 4.dp), color = muted())
            // The deck's "gap" key, where the person is when they know:
            // beside an empty list, or the notice over a list that is all loose.
            if ((p.items.isEmpty() || allLoose) && event != null && doors?.learn == true) GapButton(engram, event, q)
            Rail(rail, onOpen = { id -> onHit(id, p.event) })
        }
    }
}

/** *Nothing here has it*: a gap against the search that filled the rail, answered with the line that replaces the button. */
@Composable
private fun GapButton(engram: Engram, event: String, q: String) {
    val scope = rememberCoroutineScope()
    var said by remember(event) { mutableStateOf<String?>(null) }
    val s = said
    if (s != null) {
        Text(s, Modifier.padding(16.dp, 4.dp), style = MaterialTheme.typography.bodySmall, color = muted())
        return
    }
    OutlinedButton(onClick = {
        scope.launch {
            val r = engram.reader.call("POST", Api.searchGap(event), Api.json("q" to q), Decode.gap)
            said = when {
                r.value?.recorded == true -> "recorded as a gap: your base doesn't know this yet."
                r.value != null -> "nothing to record — that search was already judged."
                else -> r.error ?: "Server unreachable"
            }
        }
    }, modifier = Modifier.padding(16.dp, 4.dp)) { Text("Nothing here has it") }
}

/**
 * The idle column: the offer, what is due, and the line that says what the
 * base holds — the web's `_idle_foot`, one list where two used to be.
 */
@Composable
private fun Idle(engram: Engram, doors: Status?, onArtifact: (String) -> Unit, onCorpus: (String) -> Unit) {
    if (doors?.recommend != false) OfferCard(engram, onArtifact)
    DueBand(engram, onArtifact)
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
    IdleFoot(doors, onCorpus)
}

/**
 * The last line of the idle column, and the only place the base says what it
 * holds. While the base is young, what a paste becomes, in the two words the
 * line counts in.
 */
@Composable
fun IdleFoot(doors: Status?, onCorpus: (String) -> Unit) {
    val d = doors ?: return
    if (d.held.corpora == 0L) return
    val now = System.currentTimeMillis()
    val zone = ZoneId.systemDefault()
    if (d.teach) {
        Column(Modifier.padding(16.dp, 12.dp)) {
            for ((k, v) in listOf(
                "note" to "anything you keep",
                "reminder" to "“${d.examples.remind}”",
                "entry" to "“${d.examples.journal}”",
                "event" to "a date, nothing asked",
                "link" to "made on its own",
                "source" to "what you pasted, unchanged",
                "artifact" to "a passage search returns",
            )) {
                Row {
                    Text(k, Modifier.padding(end = 8.dp), style = MaterialTheme.typography.labelSmall, fontWeight = FontWeight.Medium, color = muted())
                    Text(v, style = MaterialTheme.typography.labelSmall, color = muted())
                }
            }
        }
    }
    Row(Modifier.fillMaxWidth().padding(16.dp, 8.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(
            "${d.held.artifacts} artifact${if (d.held.artifacts != 1L) "s" else ""} from ${d.held.corpora} source${if (d.held.corpora != 1L) "s" else ""}",
            style = MaterialTheme.typography.labelSmall, color = muted(),
        )
        d.lastKept?.let { k ->
            Text(" · last kept ", style = MaterialTheme.typography.labelSmall, color = muted())
            Text(
                k.label, Modifier.clickable { onCorpus(k.id) }.weight(1f, fill = false), maxLines = 1,
                style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.primary,
            )
            Text(" ${dayWords(k.at, now, zone)}", style = MaterialTheme.typography.labelSmall, color = muted())
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
    var details by remember { mutableStateOf(false) }
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
            // The parameters, visible: whoever wants to know exactly, expands it.
            if (o.rung != "random") {
                TextButton(onClick = { details = !details }) { Text(if (details) "Less" else "Details", style = MaterialTheme.typography.labelSmall) }
                if (details) Text("rung ${o.rung} · ${o.events} occasion${if (o.events != 1L) "s" else ""}", style = MaterialTheme.typography.labelSmall.copy(fontFamily = Mono), color = muted())
            }
        }
    }
}
