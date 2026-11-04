package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.read.Reminder
import java.time.Instant
import java.time.LocalDate
import java.time.ZoneId

/*
 * The reminder on an artifact, by hand: the web's editor under the text
 * (`_artifact_detail.html`), drawn for a phone. When, how it repeats, save,
 * remove; on an ordinary entry the same editor makes one.
 */

/**
 * The repetitions the editor names and the rule each one stores: the web's
 * `REPEATS`, rule for rule. Weekly, monthly and yearly carry no `BYDAY`: the
 * server takes the day from the occurrence, so moving the date moves the
 * repetition with it. Any other rule is `custom`, shown as it is stored.
 */
val REPEATS: List<Pair<String, String?>> = listOf(
    "once" to null,
    "daily" to "FREQ=DAILY",
    "weekly" to "FREQ=WEEKLY",
    "monthly" to "FREQ=MONTHLY",
    "yearly" to "FREQ=YEARLY",
)
const val CUSTOM = "custom"

/** The name a stored rule goes by in the editor. */
fun repeatOf(rule: String?): String = REPEATS.firstOrNull { it.second == rule }?.first ?: CUSTOM

/** The rule a choice stores. A custom rule is what was typed, and a blank one is none, as on the web. */
fun ruleOf(repeat: String, typed: String): String? =
    if (repeat == CUSTOM) typed.trim().ifEmpty { null } else REPEATS.firstOrNull { it.first == repeat }?.second

/** The editor's closed line: the web's `<summary>`. */
fun reminderSummary(r: Reminder?): String = when {
    r == null -> "+ reminder"
    repeatOf(r.rule) == "once" -> "reminder"
    else -> "reminder · ${repeatOf(r.rule)}"
}

/** The zone a reminder is edited in: its own where it has one this phone knows, else the phone's. */
fun zoneOf(r: Reminder?): ZoneId = r?.tz?.let { runCatching { ZoneId.of(it) }.getOrNull() } ?: ZoneId.systemDefault()

/**
 * The editor itself, with no server in it: [onSave] gets the instant, the
 * zone it was chosen in and the rule; [onRemove] is offered only where there
 * is a reminder to remove. [undo], where a save or a removal is still queued,
 * takes it back.
 */
@Composable
fun ReminderEditor(
    reminder: Reminder?,
    onSave: (at: Long, tz: String, rule: String?) -> Unit,
    onRemove: () -> Unit,
    modifier: Modifier = Modifier,
    undo: (() -> Unit)? = null,
) {
    val zone = zoneOf(reminder)
    var open by rememberSaveable(reminder) { mutableStateOf(false) }
    var at by rememberSaveable(reminder) { mutableStateOf(reminder?.at) }
    var repeat by rememberSaveable(reminder) { mutableStateOf(repeatOf(reminder?.rule)) }
    var typed by rememberSaveable(reminder) { mutableStateOf(reminder?.rule.takeIf { repeatOf(it) == CUSTOM } ?: "") }
    var picking by rememberSaveable { mutableStateOf(false) }
    Column(modifier) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = { open = !open }) { Text(reminderSummary(reminder)) }
            reminder?.at?.let { Text(fullWhen(it, zone), style = MaterialTheme.typography.labelSmall, color = due()) }
            undo?.let { TextButton(onClick = it) { Text("Undo") } }
        }
        if (open) {
            Column(Modifier.padding(horizontal = 12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                TextButton(onClick = { picking = true }) { Text(at?.let { fullWhen(it, zone) } ?: "when?") }
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    (REPEATS.map { it.first } + CUSTOM).forEach { name ->
                        FilterChip(selected = repeat == name, onClick = { repeat = name }, label = { Text(name) })
                    }
                }
                if (repeat == CUSTOM) OutlinedTextField(
                    value = typed,
                    onValueChange = { typed = it },
                    modifier = Modifier.fillMaxWidth(),
                    singleLine = true,
                    textStyle = MaterialTheme.typography.bodyMedium.copy(fontFamily = Mono),
                    placeholder = { Text("FREQ=WEEKLY;BYDAY=MO", fontFamily = Mono) },
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(onClick = { at?.let { onSave(it, zone.id, ruleOf(repeat, typed)); open = false } }, enabled = at != null) { Text("Save") }
                    if (reminder != null) TextButton(onClick = { onRemove(); open = false }) { Text("Remove") }
                }
            }
        }
    }
    if (picking) {
        val from = at?.let { Instant.ofEpochSecond(it).atZone(zone) }
        WhenDialog(
            initial = from?.toLocalDate() ?: LocalDate.now(zone),
            initialTime = from?.toLocalTime() ?: java.time.LocalTime.of(9, 0),
            zone = zone,
            onDismiss = { picking = false },
            onPick = { picking = false; at = it },
        )
    }
}
