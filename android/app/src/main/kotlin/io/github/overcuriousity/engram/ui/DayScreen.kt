package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.DayMoment
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.launch
import java.time.LocalDate
import java.time.ZoneId

/**
 * One day of the base: what was written as an entry, what was captured, what
 * was due, what refers to it, and the sittings. Read in the phone's zone, which
 * is also the zone the server is asked to cut the day in.
 */
@Composable
fun DayScreen(
    engram: Engram,
    date: LocalDate,
    onDay: (LocalDate) -> Unit,
    onCorpus: (String) -> Unit,
    onArtifact: (String) -> Unit,
) {
    val zone = ZoneId.systemDefault()
    val today = LocalDate.now(zone)
    val state = rememberRead(engram, Api.day(date.toString(), zone.id), Decode.day)
    val scope = rememberCoroutineScope()
    var entry by rememberSaveable(date) { mutableStateOf("") }
    var said by remember { mutableStateOf<String?>(null) }
    // Rows switched here, before the read shows it: an entry made a capture
    // must not sit under Entries until the row is delivered and read back.
    val switched = remember { mutableStateListOf<String>() }
    fun switchEntry(id: String, on: Boolean) {
        switched += id
        scope.launch {
            engram.outbox.enqueueCall(if (on) "Capture · made an entry" else "Entry · filed with the captures", "POST", Api.corpusEntry(id), Api.json("on" to on))
            Sync.kick(engram.app)
        }
    }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        Row(Modifier.fillMaxWidth().padding(horizontal = 4.dp), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = { onDay(date.minusDays(1)) }) { Text("‹") }
            Text(dayHeading(date), Modifier.weight(1f), textAlign = TextAlign.Center, style = MaterialTheme.typography.titleMedium)
            // No tomorrow: nothing has happened on it.
            TextButton(onClick = { onDay(date.plusDays(1)) }, enabled = date < today) { Text("›") }
        }
        // The box at the top writes an entry *into this day*, whenever it is
        // written. Pressed where the server is: the day is read back with it.
        OutlinedTextField(
            value = entry, onValueChange = { entry = it },
            modifier = Modifier.fillMaxWidth().padding(16.dp, 4.dp),
            placeholder = { Text("What happened. Kept as this day's entry.") },
            minLines = 2, maxLines = 8,
        )
        Row(Modifier.fillMaxWidth().padding(horizontal = 16.dp), horizontalArrangement = Arrangement.End, verticalAlignment = Alignment.CenterVertically) {
            said?.let { Text(it, Modifier.weight(1f), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error) }
            Button(onClick = {
                val t = entry.trim()
                scope.launch {
                    val r = engram.reader.call("POST", Api.dayEntry(date.toString()), Api.json("text" to t, "tz" to zone.id), Decode.dayEntry)
                    if (r.value != null) { entry = ""; said = null; state.retry() } else said = r.error ?: "Server unreachable"
                }
            }, enabled = entry.isNotBlank()) { Text("Keep as entry") }
        }
        // Today's page is also where what is due *now* belongs; the day's own
        // "was due" below is what fell on this date, settled or not.
        if (date == today) DueBand(engram, onArtifact, head = true)

        ReadFrame(state) { day ->
            if (day.isEmpty) Text("Nothing on this day", Modifier.padding(16.dp), color = muted())
            val entries = day.entries.filter { it.id !in switched }
            if (entries.isNotEmpty()) {
                SectionHead("Entries")
                entries.forEach { e ->
                    Column(Modifier.padding(16.dp, 6.dp)) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(clock(e.at, zone), Modifier.weight(1f), style = MaterialTheme.typography.labelMedium, color = muted())
                            TextButton(onClick = { switchEntry(e.id, false) }) { Text("Not an entry", style = MaterialTheme.typography.labelSmall) }
                        }
                        Markdown(e.text)
                    }
                }
            }
            val captured = day.captured.filter { it.id !in switched }
            if (captured.isNotEmpty()) {
                SectionHead("Captured")
                captured.forEach { c ->
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) { LinkRow(c.label, c.named, clock(c.at, zone)) { onCorpus(c.id) } }
                        TextButton(onClick = { switchEntry(c.id, true) }) { Text("Make it an entry", style = MaterialTheme.typography.labelSmall) }
                    }
                }
            }
            if (day.wasDue.isNotEmpty()) {
                SectionHead("Was due")
                day.wasDue.forEach { m -> MomentLine(m, zone, if (m.done) "done" else "still open", onArtifact) }
            }
            if (day.refers.isNotEmpty()) {
                SectionHead("Refers to this day")
                day.refers.forEach { m -> MomentLine(m, zone, m.span?.let { "“$it”" } ?: "", onArtifact) }
            }
            if (day.sittings.isNotEmpty()) {
                SectionHead("Sittings")
                day.sittings.forEach { s ->
                    Column(Modifier.padding(top = 6.dp)) {
                        Text(
                            "${clock(s.openedAt, zone)}–${clock(s.closedAt, zone)} · ${s.query} · ${s.searches} search${if (s.searches == 1) "" else "es"}",
                            Modifier.padding(horizontal = 16.dp), style = MaterialTheme.typography.bodyMedium,
                        )
                        s.opened.forEach { o -> LinkRow(o.label, o.named) { onArtifact(o.id) } }
                    }
                }
            }
        }
    }
}

@Composable
private fun MomentLine(m: DayMoment, zone: ZoneId, detail: String, onArtifact: (String) -> Unit) {
    val trailing = listOfNotNull(m.at?.let { clock(it, zone) }, detail.takeIf { it.isNotEmpty() }).joinToString(" · ")
    LinkRow(m.label, m.named, trailing) { onArtifact(m.artifactId) }
}
