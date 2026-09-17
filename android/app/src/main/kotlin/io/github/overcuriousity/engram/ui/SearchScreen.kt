package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.DueRow
import io.github.overcuriousity.engram.core.read.Offer
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.launch
import java.time.ZoneId

/**
 * Home. A box, and beneath an empty one what the base has to say unasked: the
 * offer for this situation, what is due, what is worth seeing again.
 *
 * A search runs when it is asked for — the button, or the keyboard's search
 * key — and not on every keystroke. The web searches as you type because its
 * embedding call is a loopback away; from a phone each keystroke would be an
 * embedding call across a VPN, racing the one before it.
 */
@Composable
fun SearchScreen(engram: Engram, onArtifact: (String) -> Unit, onAsk: (String) -> Unit) {
    var text by rememberSaveable { mutableStateOf("") }
    var asked by rememberSaveable { mutableStateOf("") }
    val keyboard = LocalSoftwareKeyboardController.current
    fun search() { asked = text.trim(); keyboard?.hide() }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        OutlinedTextField(
            value = text,
            onValueChange = { text = it; if (it.isBlank()) asked = "" },
            modifier = Modifier.fillMaxWidth().padding(16.dp, 8.dp),
            placeholder = { Text("Search or ask") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
            keyboardActions = KeyboardActions(onSearch = { search() }),
        )
        Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp), horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End)) {
            OutlinedButton(onClick = { onAsk(text.trim()) }, enabled = text.isNotBlank()) { Text("Ask") }
            Button(onClick = ::search, enabled = text.isNotBlank()) { Text("Search") }
        }
        if (asked.isEmpty()) Idle(engram, onArtifact) else Results(engram, asked, onArtifact)
    }
}

@Composable
private fun Results(engram: Engram, q: String, onArtifact: (String) -> Unit) {
    val state = rememberRead(engram, Api.search(q), Decode.hits)
    ReadFrame(state) { page ->
        if (page.items.isEmpty()) Text("Nothing", Modifier.padding(16.dp), color = muted())
        Rail(railOf(page.items), onArtifact)
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
