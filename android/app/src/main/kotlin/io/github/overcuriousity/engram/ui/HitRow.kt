package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp

/** One result. Past the rule it is drawn back: it placed, and it is not claimed as an answer. */
@Composable
fun HitRow(row: RailItem.Row, onOpen: (String) -> Unit, items: List<RailItem> = emptyList()) {
    val h = row.hit
    val (name, named) = nameOf(h)
    Column(
        Modifier.fillMaxWidth().clickable { onOpen(h.artifactId) }.alpha(if (row.past) 0.62f else 1f).padding(16.dp, 10.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            when {
                row.loose -> Badge("loose", MaterialTheme.colorScheme.secondary)
                row.rank != null -> Text("#${row.rank}", style = MaterialTheme.typography.labelMedium, color = muted())
            }
            if (row.loose || row.rank != null) Spacer(Modifier.width(8.dp))
            // A row with no name is its own opening, and gets the room a
            // snippet would have had: one clipped line told nobody what it was.
            if (named) Label(name, true, maxLines = 1) else Label(opening(h.text, 160), false, maxLines = 3)
        }
        // The snippet, unless the label already is the opening of the text.
        if (named) {
            Text(
                opening(h.text, 160), Modifier.padding(top = 2.dp), maxLines = 3, overflow = TextOverflow.Ellipsis,
                style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        sectionOf(h)?.let { Text("in $it", Modifier.padding(top = 2.dp), style = MaterialTheme.typography.labelSmall, color = muted()) }
        val words = wordsOf(h)
        if (words.isNotEmpty()) {
            Text(words.joinToString(" · "), Modifier.padding(top = 4.dp), style = MaterialTheme.typography.labelSmall, color = if (h.dueIn != null) due() else muted())
        }
        val allLoose = items.any { it == RailItem.NothingClose }
        whyOf(h, allLoose)?.let { Text(it, Modifier.padding(top = 2.dp), style = MaterialTheme.typography.labelSmall, color = muted()) }
        // Its own line, under the ones that say why this row is here: what
        // the document does next is something else.
        continuesWords(h, items)?.let { Text(it, Modifier.padding(top = 2.dp), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.primary) }
    }
}

@Composable
fun Badge(text: String, color: androidx.compose.ui.graphics.Color) {
    Box(Modifier.background(color.copy(alpha = 0.14f), MaterialTheme.shapes.small).padding(6.dp, 1.dp)) {
        Text(text, style = MaterialTheme.typography.labelSmall, color = color)
    }
}

/** *Relevance falls off here.* A separator a screen reader announces, not a decoration. */
@Composable
fun CliffRule() {
    Row(Modifier.fillMaxWidth().padding(16.dp, 10.dp).semantics { heading() }, verticalAlignment = Alignment.CenterVertically) {
        HorizontalDivider(Modifier.weight(1f), color = MaterialTheme.colorScheme.outline)
        Text("Relevance falls off here", Modifier.padding(horizontal = 10.dp), style = MaterialTheme.typography.labelSmall, color = muted())
        HorizontalDivider(Modifier.weight(1f), color = MaterialTheme.colorScheme.outline)
    }
}

@Composable
fun NothingClose() {
    Column(Modifier.fillMaxWidth().padding(16.dp, 8.dp).background(MaterialTheme.colorScheme.secondary.copy(alpha = 0.10f), MaterialTheme.shapes.medium).padding(12.dp)) {
        Text("Nothing matches closely", style = MaterialTheme.typography.labelLarge)
        Spacer(Modifier.height(2.dp))
        Text("The nearest artifacts, none of them close", style = MaterialTheme.typography.bodySmall, color = muted())
    }
}

/** A whole list of hits: the rows, the rule, and the notice, as `railOf` lays them out. */
@Composable
fun Rail(items: List<RailItem>, onOpen: (String) -> Unit) {
    Column {
        items.forEach { item ->
            when (item) {
                is RailItem.Row -> HitRow(item, onOpen, items)
                RailItem.Cliff -> CliffRule()
                RailItem.NothingClose -> NothingClose()
            }
        }
    }
}
