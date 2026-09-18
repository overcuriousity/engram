package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.core.AskVia
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.contained.ModelManifest
import io.github.overcuriousity.engram.core.contained.Role
import io.github.overcuriousity.engram.core.ask.AskState
import io.github.overcuriousity.engram.core.ask.Phase
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.AskAnswer
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.Hit
import io.github.overcuriousity.engram.core.read.Kept
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import java.time.ZoneId

/**
 * Ask. Unlike every other screen this one is not a read: the answer arrives as
 * a stream, is drawn as it grows, and is then drawn again — whole, from the
 * server, as markdown with its citations linked and the literals no excerpt
 * carries marked. All of that is `AskState`; this draws it, with everything the
 * web's answer card has under it: the badges, the verdict bar, keeping the
 * answer or editing it first, and the excerpts with *carried the answer*.
 *
 * Leaving the screen cancels the collection, which closes the call: an ask
 * nobody is reading would otherwise hold the server's lane until its timeout.
 * So does Stop, which is the same act by hand — the tokens that arrived stay.
 */
@Composable
fun AskScreen(
    engram: Engram,
    initial: String,
    onArtifact: (String) -> Unit,
    onCorpus: (String) -> Unit,
    onEditFirst: (answer: String, event: String, question: String) -> Unit,
    onSettings: () -> Unit = {},
) {
    val scope = rememberCoroutineScope()
    var text by rememberSaveable { mutableStateOf(initial) }
    var asking by rememberSaveable { mutableStateOf(initial) }
    var state by remember { mutableStateOf<AskState?>(null) }
    var job by remember { mutableStateOf<Job?>(null) }
    val keyboard = LocalSoftwareKeyboardController.current
    val history by engram.ask.history.collectAsStateWithLifecycle(emptyList())

    // Counts presses of Ask. Zero means the question arrived with the screen —
    // from the search box, or restored after a rotation — and then an answer
    // already kept is shown rather than asked for again: an ask is a model
    // call, and turning the phone must not cost one.
    var presses by remember { mutableIntStateOf(0) }
    LaunchedEffect(asking, presses) {
        job?.cancel()
        state = null
        if (asking.isBlank()) return@LaunchedEffect
        val kept = if (presses == 0) engram.ask.kept(asking) else null
        if (kept != null) state = AskState(kept.question, Phase.Done, answer = kept.answer, citations = kept.answer.citations)
        else job = scope.launch { engram.ask.run(asking).collect { state = it } }
    }
    fun ask() { asking = text.trim(); presses++; keyboard?.hide() }
    val working = state?.phase.let { it == Phase.Retrieving || it == Phase.Writing }

    // On a phone that is its own engram, Ask may have nothing to answer with.
    // The offer stands where the box would, once, and nowhere else.
    var looked by remember { mutableIntStateOf(0) }
    val off = remember(looked) { engram.loopback && engram.modes.ask == AskVia.off }
    val wants = remember(looked) { engram.askWantsAModel }
    if (off) { AskOffPane(onSettings); return }
    if (wants) {
        AskOfferPane(
            models = {
                ModelManifest.all.filter { it.role == Role.ask }.forEach { m ->
                    ModelLine(engram, m, onChanged = { scope.launch { engram.restartCore(); looked++ } })
                }
            },
            onEndpoint = onSettings,
            onOff = { engram.modes.ask = AskVia.off; looked++ },
        )
        return
    }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        OutlinedTextField(
            value = text, onValueChange = { text = it },
            modifier = Modifier.fillMaxWidth().padding(16.dp, 8.dp),
            placeholder = { Text("A question") },
            keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
            keyboardActions = KeyboardActions(onSend = { ask() }),
            maxLines = 4,
        )
        Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp), horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End)) {
            // A way out of a wait whose length nothing on the page predicts.
            if (working) TextButton(onClick = {
                job?.cancel()
                state = state?.copy(phase = Phase.Failed, error = "stopped")
            }) { Text("Stop") }
            Button(onClick = ::ask, enabled = text.isNotBlank() && !working) { Text("Ask") }
        }

        val s = state
        if (s != null) Answer(engram, s, onArtifact, onCorpus, onEditFirst)
        else if (history.isNotEmpty()) {
            SectionHead("Earlier")
            history.forEach { k ->
                LinkRow(k.question, named = true, trailing = dayWords(k.askedAt / 1000, System.currentTimeMillis(), ZoneId.systemDefault())) {
                    // Read from what was kept, without the server.
                    text = k.question
                    state = AskState(k.question, Phase.Done, answer = k.answer, citations = k.answer.citations)
                }
            }
        }
    }
}

/** The badges over an answer, in the web's words: each a fact about the answer a reader should know before relying on it. */
fun answerBadges(a: AskAnswer): List<String> = buildList {
    if (a.abstained) add("nothing here")
    if (a.truncated) add("cut off at the answer length limit")
    if (a.unsupported.isNotEmpty()) add("${a.unsupported.size} literal${if (a.unsupported.size != 1) "s" else ""} no excerpt supports")
    if (a.retiredOnly) add("written only from retired notes")
    if (a.dropped > 0) add("${a.dropped} more excerpt${if (a.dropped != 1) "s" else ""} did not fit")
}

@Composable
private fun Answer(
    engram: Engram,
    s: AskState,
    onArtifact: (String) -> Unit,
    onCorpus: (String) -> Unit,
    onEditFirst: (String, String, String) -> Unit,
) {
    val scope = rememberCoroutineScope()
    val working = s.phase == Phase.Retrieving || s.phase == Phase.Writing
    if (working) LinearProgressIndicator(Modifier.fillMaxWidth().padding(top = 8.dp))
    // What the retrieval did before the model was asked anything, when there
    // was more of it than usual.
    if (s.phase == Phase.Retrieving) {
        val read = s.shown?.let { "reading $it" + (s.dropped?.takeIf { d -> d > 0 }?.let { d -> " · $d left out" } ?: "") } ?: "retrieving"
        Text(read, Modifier.padding(16.dp, 8.dp), style = MaterialTheme.typography.labelSmall, color = muted())
    }
    if (s.needs.isNotEmpty() && working) {
        Text("also looking for " + s.needs.joinToString(", "), Modifier.padding(16.dp, 0.dp), style = MaterialTheme.typography.labelSmall, color = muted())
    }
    // What the model said on the way to the answer: behind a disclosure and
    // closed. Not the answer, nothing in it is cited.
    if (s.reasoning.isNotBlank()) Column(Modifier.padding(16.dp, 4.dp)) {
        Fold("Reasoning") { Text(s.reasoning, style = MaterialTheme.typography.bodySmall, color = muted()) }
    }

    val answer = s.answer
    Surface(color = MaterialTheme.colorScheme.surface, shape = MaterialTheme.shapes.medium, modifier = Modifier.fillMaxWidth().padding(16.dp, 8.dp)) {
        Column(Modifier.padding(14.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) { Text("Answer", style = MaterialTheme.typography.labelLarge) }
            if (answer != null) {
                val badges = answerBadges(answer)
                if (badges.isNotEmpty()) FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(4.dp), modifier = Modifier.padding(top = 4.dp)) {
                    badges.forEach { Badge(it, MaterialTheme.colorScheme.secondary) }
                }
            }
            // While it streams: the draft, plain. Once it is done: the server's
            // whole answer in its place, as markdown, with what no excerpt
            // carries marked and every `[n]` a link to its excerpt. The second
            // drawing is the point.
            if (answer == null) {
                if (s.draft.isNotEmpty()) SelectionContainer { Text(s.draft, Modifier.padding(top = 8.dp), style = MaterialTheme.typography.bodyLarge) }
            } else if (answer.answer.isNotBlank()) {
                Markdown(
                    citeLinks(answer.answer, s.citations.size),
                    Modifier.padding(top = 8.dp),
                    mark = answer.unsupported,
                    onCite = { n -> s.citations.getOrNull(n - 1)?.let { onArtifact(it.artifactId) } },
                )
            }
            s.error?.let { Text(it, Modifier.padding(top = 4.dp), color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
            // Judged where it is read, and kept from where it is read. Present
            // only when the question was recorded.
            val ev = answer?.eventId
            if (answer != null && ev != null) {
                var verdict by remember(ev) { mutableStateOf(AskVerdict()) }
                AskVerdictBar(verdict) { w ->
                    scope.launch {
                        val r = engram.reader.call("POST", Api.askVerdict(ev), Api.json("verdict" to w), Decode.askVerdict)
                        verdict = r.value?.let { AskVerdict(it.verdict) } ?: AskVerdict(error = r.error ?: "Server unreachable")
                    }
                }
                var kept by remember(ev) { mutableStateOf<Kept?>(null) }
                var keepSaid by remember(ev) { mutableStateOf<String?>(null) }
                val k = kept
                FlowRow(horizontalArrangement = Arrangement.spacedBy(4.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
                    when {
                        k != null -> {
                            Text(
                                when {
                                    k.duplicate -> "already in the base"
                                    k.parked -> "stored — waiting on a decision · ${k.nearDupePercent}% like an existing source"
                                    else -> "kept"
                                },
                                Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.tertiary,
                            )
                            TextButton(onClick = { onCorpus(k.id) }) { Text("view source") }
                        }
                        else -> {
                            // A button, not a link to the capture box: keeping an
                            // answer stores it, here, as an ordinary source.
                            TextButton(onClick = {
                                scope.launch {
                                    val r = engram.reader.call("POST", Api.askKeep(ev), null, Decode.kept)
                                    if (r.value != null) kept = r.value else keepSaid = r.error ?: "Server unreachable"
                                }
                            }) { Text("Keep this answer") }
                            TextButton(onClick = { onEditFirst(answer.answer, ev, s.question) }) { Text("edit first") }
                            keepSaid?.let { Text(it, Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error) }
                        }
                    }
                }
            }
        }
    }

    if (s.citations.isNotEmpty()) {
        SectionHead("Artifacts used")
        val carried = remember(answer?.eventId) { mutableStateMapOf<Int, Boolean>() }
        s.citations.forEachIndexed { i, h -> Citation(engram, i + 1, h, answer?.eventId, carried, onArtifact, onCorpus) }
    }
}

/** One excerpt the answer was written from: its name, its rank, its text, and whether it carried the answer. */
@Composable
private fun Citation(
    engram: Engram,
    n: Int,
    h: Hit,
    event: String?,
    carried: MutableMap<Int, Boolean>,
    onArtifact: (String) -> Unit,
    onCorpus: (String) -> Unit,
) {
    val scope = rememberCoroutineScope()
    val (name, named) = nameOf(h)
    Surface(color = MaterialTheme.colorScheme.surface, shape = MaterialTheme.shapes.medium, modifier = Modifier.fillMaxWidth().padding(16.dp, 6.dp)) {
        Column(Modifier.padding(14.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("[$n]", Modifier.padding(end = 8.dp), style = MaterialTheme.typography.labelMedium, color = muted())
                if (named) Label(name, true, Modifier.weight(1f), maxLines = 1)
            }
            Markdown(h.text, Modifier.padding(top = 6.dp), style = MaterialTheme.typography.bodyMedium)
            FlowRow(horizontalArrangement = Arrangement.spacedBy(4.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
                if (event != null) {
                    val on = carried[n] == true
                    TextButton(onClick = {
                        scope.launch {
                            val r = engram.reader.call("POST", Api.askCarried(event), Api.json("n" to n), Decode.carried)
                            r.value?.let { carried[n] = it.carried }
                        }
                    }) { Text(if (on) "✓ carried the answer" else "carried the answer") }
                }
                TextButton(onClick = { onArtifact(h.artifactId) }) { Text("open") }
                if (h.corpusId.isNotEmpty()) TextButton(onClick = { onCorpus(h.corpusId) }) { Text("source") }
            }
        }
    }
}
