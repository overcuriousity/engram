package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Column
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
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.ask.AskState
import io.github.overcuriousity.engram.core.ask.Phase
import io.github.overcuriousity.engram.core.ask.annotate
import java.time.ZoneId

/**
 * Ask. Unlike every other screen this one is not a read: the answer arrives as
 * a stream, is drawn as it grows, and is then drawn again — whole, from the
 * server, with the commands and paths no excerpt carries marked. All of that is
 * `AskState`; this draws it.
 *
 * Leaving the screen cancels the collection, which closes the call: an ask
 * nobody is reading would otherwise hold the server's lane until its timeout.
 */
@Composable
fun AskScreen(engram: Engram, initial: String, onArtifact: (String) -> Unit) {
    var text by rememberSaveable { mutableStateOf(initial) }
    var asking by rememberSaveable { mutableStateOf(initial) }
    var state by remember { mutableStateOf<AskState?>(null) }
    val keyboard = LocalSoftwareKeyboardController.current
    val history by engram.ask.history.collectAsStateWithLifecycle(emptyList())

    // Counts presses of Ask. Zero means the question arrived with the screen —
    // from the search box, or restored after a rotation — and then an answer
    // already kept is shown rather than asked for again: an ask is a model
    // call, and turning the phone must not cost one.
    var presses by remember { mutableIntStateOf(0) }
    LaunchedEffect(asking, presses) {
        state = null
        if (asking.isBlank()) return@LaunchedEffect
        val kept = if (presses == 0) engram.ask.kept(asking) else null
        if (kept != null) state = AskState(kept.question, Phase.Done, answer = kept.answer, citations = kept.answer.citations)
        else engram.ask.run(asking).collect { state = it }
    }
    fun ask() { asking = text.trim(); presses++; keyboard?.hide() }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        OutlinedTextField(
            value = text, onValueChange = { text = it },
            modifier = Modifier.fillMaxWidth().padding(16.dp, 8.dp),
            placeholder = { Text("A question") },
            keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
            keyboardActions = KeyboardActions(onSend = { ask() }),
            maxLines = 4,
        )
        Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp), horizontalArrangement = androidx.compose.foundation.layout.Arrangement.End) {
            Button(onClick = ::ask, enabled = text.isNotBlank() && state?.phase.let { it == null || it == Phase.Done || it == Phase.Failed }) { Text("Ask") }
        }

        val s = state
        if (s != null) Answer(s, onArtifact)
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

@Composable
private fun Answer(s: AskState, onArtifact: (String) -> Unit) {
    val working = s.phase == Phase.Retrieving || s.phase == Phase.Writing
    if (working) LinearProgressIndicator(Modifier.fillMaxWidth().padding(top = 8.dp))
    if (s.phase == Phase.Retrieving) {
        val read = s.shown?.let { "reading $it" + (s.dropped?.takeIf { d -> d > 0 }?.let { d -> " · $d left out" } ?: "") } ?: "retrieving"
        Text(read, Modifier.padding(16.dp, 8.dp), style = MaterialTheme.typography.labelSmall, color = muted())
    }
    if (s.needs.isNotEmpty() && working) {
        Text("also looking for " + s.needs.joinToString(", "), Modifier.padding(16.dp, 0.dp), style = MaterialTheme.typography.labelSmall, color = muted())
    }

    val warn = MaterialTheme.colorScheme.secondary
    val answer = s.answer
    // While it streams: the draft, plain. Once it is done: the server's whole
    // answer in its place, annotated. The second drawing is the point.
    val body: AnnotatedString = if (answer == null) AnnotatedString(s.draft) else marked(answer.answer, answer.unsupported, warn)
    if (body.isNotEmpty()) {
        SelectionContainer { Text(body, Modifier.padding(16.dp, 12.dp), style = MaterialTheme.typography.bodyLarge) }
    }
    s.error?.let { Text(it, Modifier.padding(16.dp, 4.dp), color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }

    if (answer != null) {
        val notes = buildList {
            if (answer.unsupported.isNotEmpty()) add("marked: not in any excerpt")
            if (answer.truncated) add("cut off at the length limit")
            if (answer.retiredOnly) add("from retired notes only")
            if (answer.dropped > 0) add("${answer.dropped} left out")
        }
        notes.forEachIndexed { i, n ->
            Text(n, Modifier.padding(16.dp, 1.dp), style = MaterialTheme.typography.labelSmall, color = if (i == 0 && answer.unsupported.isNotEmpty()) warn else muted())
        }
    }
    if (s.citations.isNotEmpty()) {
        SectionHead("Read for this")
        s.citations.forEachIndexed { i, h ->
            val (name, named) = nameOf(h)
            Row(verticalAlignment = Alignment.CenterVertically) {
                LinkRow("${i + 1}  $name", named) { onArtifact(h.artifactId) }
            }
        }
    }
}

/** The answer with what no excerpt carries marked: coloured, and underlined so the mark survives without colour. */
private fun marked(answer: String, unsupported: List<String>, warn: Color): AnnotatedString = buildAnnotatedString {
    annotate(answer, unsupported).forEach { run ->
        if (run.unsupported) withStyle(SpanStyle(color = warn, textDecoration = TextDecoration.Underline)) { append(run.text) }
        else append(run.text)
    }
}
