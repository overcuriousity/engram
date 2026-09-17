package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.DayMoment
import io.github.overcuriousity.engram.core.read.Decode
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

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        Row(Modifier.fillMaxWidth().padding(horizontal = 4.dp), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
            TextButton(onClick = { onDay(date.minusDays(1)) }) { Text("‹") }
            Text(dayHeading(date), Modifier.weight(1f), textAlign = TextAlign.Center, style = MaterialTheme.typography.titleMedium)
            // No tomorrow: nothing has happened on it.
            TextButton(onClick = { onDay(date.plusDays(1)) }, enabled = date < today) { Text("›") }
        }
        // Today's page is also where what is due *now* belongs; the day's own
        // "was due" below is what fell on this date, settled or not.
        if (date == today) DueList(engram, onArtifact)

        ReadFrame(state) { day ->
            if (day.isEmpty) Text("Nothing on this day", Modifier.padding(16.dp), color = muted())
            if (day.entries.isNotEmpty()) {
                SectionHead("Entries")
                day.entries.forEach { e ->
                    Column(Modifier.padding(16.dp, 6.dp)) {
                        Text(clock(e.at, zone), style = MaterialTheme.typography.labelMedium, color = muted())
                        Text(e.text, style = MaterialTheme.typography.bodyLarge)
                    }
                }
            }
            if (day.captured.isNotEmpty()) {
                SectionHead("Captured")
                day.captured.forEach { c -> LinkRow(c.label, c.named, clock(c.at, zone)) { onCorpus(c.id) } }
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
