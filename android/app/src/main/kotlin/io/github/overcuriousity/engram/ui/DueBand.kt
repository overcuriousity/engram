package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DatePicker
import androidx.compose.material3.DatePickerDialog
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TimePicker
import androidx.compose.material3.rememberDatePickerState
import androidx.compose.material3.rememberTimePickerState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.DueRow
import io.github.overcuriousity.engram.core.read.Moment
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.launch
import java.time.Instant
import java.time.LocalDate
import java.time.LocalTime
import java.time.ZoneId
import java.time.ZoneOffset

/** How many rows the band shows before it folds, as the web's does. */
internal const val DUE_FOLD = 5

/** How far ahead *coming up* looks. The web asks its server's `time.coming_up_days`; this is the shipped default. */
internal const val COMING_UP_DAYS = 14L

/**
 * The band under the offer: what is due, with the actions the web's band has
 * — done and its undo, three snoozes, a date to set or move, and *not a
 * reminder* on a row the stage read rather than a person set — the fold past
 * five rows, and what is coming up.
 *
 * Every write here is an outbox row, so a Done on a train is a Done, and the
 * undo is the row taken back out while it is still queued. *Not a reminder*
 * is the one press answered at once: whether it can be taken back is the
 * server's to say, and the web asks the same question the same way.
 */
@Composable
fun DueBand(engram: Engram, onArtifact: (String) -> Unit, head: Boolean = false) {
    val due = rememberRead(engram, Api.due(), Decode.due)
    val now = System.currentTimeMillis()
    val zone = ZoneId.systemDefault()
    val events = rememberRead(engram, Api.events(now / 1000, now / 1000 + COMING_UP_DAYS * 86_400), Decode.due)
    var all by remember { mutableStateOf(false) }
    // What this band has already settled, held above the rows — see [Settled].
    val settled = remember { Settled() }
    ReadFrame(due) { page ->
        val coming = events.read.value?.items.orEmpty()
        if (page.items.isEmpty() && coming.isEmpty()) return@ReadFrame
        if (head && page.items.isNotEmpty()) SectionHead("Due")
        val shown = if (all) page.items else page.items.take(DUE_FOLD)
        shown.forEach { key(it.moment.id) { DueLine(engram, it, settled, onArtifact) } }
        val hidden = page.items.size - shown.size
        if (hidden > 0) TextButton(onClick = { all = true }, modifier = Modifier.padding(start = 8.dp)) { Text("$hidden more · show all") }
        else if (all && page.items.size > DUE_FOLD) TextButton(onClick = { all = false }, modifier = Modifier.padding(start = 8.dp)) { Text("Show less") }
        if (coming.isNotEmpty()) {
            Text("Coming up", Modifier.padding(16.dp, 6.dp), style = MaterialTheme.typography.labelSmall, color = muted())
            coming.forEach { r ->
                val words = listOfNotNull(r.moment.at?.let { dueWords(it, now) }, r.moment.span?.let { "“$it”" }).joinToString(" · ")
                LinkRow(r.title, r.named, words) { onArtifact(r.moment.artifactId) }
            }
        }
    }
}

/** The list alone, for the screens that had it before the band: the same rows and actions, no fold and no events. */
@Composable
fun DueList(engram: Engram, onArtifact: (String) -> Unit) = DueBand(engram, onArtifact, head = true)

/** What was just done to a row, shown struck through with its own undo — for this render only. */
internal data class Just(val verb: String, val row: String?)

/**
 * What the band has settled, by moment: a row struck through and its undo,
 * and the rows taken out of the band altogether.
 *
 * It belongs to the band and not to the row. A row's own `remember` is held
 * by the slot it was composed in, not by the moment it was drawn from, so a
 * list that comes back reordered hands slot zero's memory to a different
 * moment: the row that was settled comes back unsettled — and can be Done a
 * second time — while its struck-through line sits on somebody else's. One
 * map above the rows, keyed by the moment, is what the rows were promised.
 */
@Stable
internal class Settled {
    /** Struck through, awaiting its outbox row. */
    val just = mutableStateMapOf<String, Just>()

    /** Not a reminder: gone from the band, the server having said so. */
    val gone = mutableStateListOf<String>()
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun DueLine(engram: Engram, row: DueRow, settled: Settled, onArtifact: (String) -> Unit) {
    val scope = rememberCoroutineScope()
    val now = System.currentTimeMillis()
    val zone = ZoneId.systemDefault()
    val m = row.moment
    // Kept across reads, keyed by the moment, and held by the band rather than
    // by this row: a settled row must not come back when the list is
    // revalidated before the write lands, whatever order it comes back in.
    val just = settled.just
    val gone = settled.gone
    var later by remember { mutableStateOf(false) }
    var picking by remember { mutableStateOf(false) }
    val read = m.source != "set"
    val undated = row.at == null

    fun owe(verb: String, write: suspend () -> String) {
        scope.launch {
            val id = write()
            just[m.id] = Just(verb, id)
            Sync.kick(engram.app)
        }
    }
    just[m.id]?.let { j ->
        Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(j.verb, Modifier.weight(1f), style = MaterialTheme.typography.bodyMedium, color = muted(), textDecoration = TextDecoration.LineThrough)
            j.row?.let { r -> TextButton(onClick = { scope.launch { if (engram.outbox.undo(r)) just.remove(m.id) } }) { Text("Undo") } }
        }
        return
    }
    if (m.id in gone) return

    Column(Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp, top = 4.dp, bottom = 4.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f).clickable { onArtifact(m.artifactId) }.padding(vertical = 4.dp)) {
                Label(row.title, row.named, maxLines = 1)
                val words = buildList {
                    row.at?.let { add(dueWords(it, now)); add(fullWhen(it, zone)) }
                    if (m.rule != null) add("repeats ↻")
                    if (read) add("read from the note")
                }
                if (words.isNotEmpty()) Text(words.joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = if (row.at != null) due() else muted())
            }
            TextButton(onClick = { owe("Done") { engram.outbox.enqueueDone(m.id) } }) { Text("Done") }
            if (!undated) TextButton(onClick = { later = !later }) { Text("later") }
        }
        // A row with no date is asking for one: the field is the whole of what
        // the row is for, and it is not behind a fold.
        if (undated || later) {
            FlowRow(horizontalArrangement = Arrangement.spacedBy(4.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
                if (!undated) {
                    Text("snooze", Modifier.padding(top = 12.dp), style = MaterialTheme.typography.labelSmall, color = muted())
                    for ((label, word) in listOf("1h" to "hour", "Tomorrow" to "tomorrow", "Monday" to "monday")) {
                        TextButton(onClick = {
                            snoozeUntil(word, now, zone)?.let { until -> owe("Snoozed until ${fullWhen(until, zone)}") { engram.outbox.enqueueSnooze(m.id, until) } }
                        }) { Text(label) }
                    }
                }
                TextButton(onClick = { picking = true }) { Text(if (undated) "Set date" else "Move") }
                // The row most likely to be a misreading is the one asking for
                // a date it never had, so this sits beside the field. A
                // reminder somebody set themselves is not offered it.
                if (read) TextButton(onClick = {
                    scope.launch {
                        val r = engram.reader.call("POST", Api.notAReminder(m.id), null, Decode.notAReminder)
                        if (r.value != null) gone += m.id
                    }
                }) { Text("Not a reminder") }
            }
        }
    }
    if (picking) WhenDialog(
        initial = row.at?.let { Instant.ofEpochSecond(it).atZone(zone) }?.toLocalDate() ?: LocalDate.now(zone),
        onDismiss = { picking = false },
        onPick = { at ->
            picking = false
            owe(if (undated) "Dated ${fullWhen(at, zone)}" else "Moved to ${fullWhen(at, zone)}") {
                engram.outbox.enqueueCall("Reminder · ${if (undated) "dated" else "moved"}", "POST", Api.momentDate(m.id), Api.json("at" to at, "tz" to zone.id))
            }
        },
    )
}

/** A day, then a time: the web's `datetime-local` in two steps. Answers Unix seconds in the phone's zone. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun WhenDialog(initial: LocalDate, onDismiss: () -> Unit, onPick: (Long) -> Unit) {
    val zone = ZoneId.systemDefault()
    val date = rememberDatePickerState(initialSelectedDateMillis = initial.atStartOfDay(ZoneOffset.UTC).toInstant().toEpochMilli())
    val time = rememberTimePickerState(initialHour = 9, initialMinute = 0, is24Hour = true)
    var day by remember { mutableStateOf<LocalDate?>(null) }
    if (day == null) {
        DatePickerDialog(
            onDismissRequest = onDismiss,
            confirmButton = {
                TextButton(onClick = {
                    date.selectedDateMillis?.let { day = Instant.ofEpochMilli(it).atZone(ZoneOffset.UTC).toLocalDate() }
                }) { Text("Next") }
            },
            dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
        ) { DatePicker(state = date) }
    } else {
        AlertDialog(
            onDismissRequest = onDismiss,
            title = { Text("At what time?") },
            text = { TimePicker(state = time) },
            confirmButton = {
                TextButton(onClick = {
                    onPick(day!!.atTime(LocalTime.of(time.hour, time.minute)).atZone(zone).toEpochSecond())
                }) { Text("Set") }
            },
            dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
        )
    }
}

/** The line a moment gets in a list that is not the band: its own words, no actions. */
fun momentWords(m: Moment, nowMs: Long): String = listOfNotNull(
    (m.snoozedUntil ?: m.at)?.let { dueWords(it, nowMs) },
    "repeats".takeIf { m.rule != null },
    "read".takeIf { m.source != "set" },
).joinToString(" · ")
