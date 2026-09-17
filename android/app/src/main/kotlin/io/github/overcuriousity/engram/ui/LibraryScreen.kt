package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.CorpusRow
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.Reach
import java.time.ZoneId

/**
 * Everything captured, newest first, a page at a time. `cursors` is the list
 * of pages asked for so far — `null` is the first — and each page is a read of
 * its own, so each is kept and revalidated on its own and a page opened before
 * can be shown again without the server. The next page is asked for when the
 * last row of the last page comes on screen.
 */
@Composable
fun LibraryScreen(engram: Engram, onCorpus: (String) -> Unit) {
    val cursors = remember { mutableStateListOf<String?>(null) }
    val zone = ZoneId.systemDefault()
    val now = System.currentTimeMillis()

    // Each page's rows and what follows it, filled in by the page's own read.
    val pages = remember { mutableStateListOf<List<CorpusRow>>() }
    var next by remember { mutableStateOf<String?>(null) }
    var unreachableAt by remember { mutableStateOf<Long?>(null) }
    var unreachable by remember { mutableStateOf(false) }
    var attempt by remember { mutableStateOf(0) }

    cursors.forEachIndexed { i, cursor ->
        key(cursor) { LaunchedEffect(cursor, attempt) {
            engram.reader.read(Api.corpora(cursor), Decode.corpora).collect { r ->
                r.value?.let { page ->
                    if (i < pages.size) pages[i] = page.items else pages.add(page.items)
                    if (i == cursors.lastIndex) next = page.next
                }
                if (!r.loading) {
                    unreachable = r.reach == Reach.Unreachable
                    unreachableAt = r.fetchedAt
                }
            }
        } }
    }

    val rows = pages.flatten()
    Column(Modifier.fillMaxSize()) {
        if (unreachable) Unreachable(unreachableAt) { attempt++ }
        if (rows.isEmpty() && !unreachable) Text("Nothing captured yet", Modifier.padding(16.dp), color = muted())
        LazyColumn {
            itemsIndexed(rows, key = { _, r -> r.id }) { i, r ->
                val state = r.status.takeIf { it != "complete" && it.isNotEmpty() }
                LinkRow(r.label, r.named, listOfNotNull(state, dayWords(r.createdAt, now, zone)).joinToString(" · ")) { onCorpus(r.id) }
                if (i == rows.lastIndex) {
                    val n = next
                    LaunchedEffect(n) { if (n != null && n !in cursors) cursors.add(n) }
                }
            }
        }
    }
}

/** One captured document: what it is called, where it came from, its text, and what was made of it. */
@Composable
fun CorpusScreen(engram: Engram, id: String, onArtifact: (String) -> Unit) {
    val state = rememberRead(engram, Api.corpus(id), Decode.corpus)
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        ReadFrame(state) { c ->
            Column(Modifier.padding(16.dp, 8.dp)) {
                Label(c.title ?: opening(c.text, 60).ifEmpty { c.origin }, named = c.title != null)
                val from = listOfNotNull(c.origin.takeIf { it.isNotEmpty() }, dayWords(c.createdAt, System.currentTimeMillis(), ZoneId.systemDefault()), c.status.takeIf { it != "complete" && it.isNotEmpty() })
                Text(from.joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = muted())
                c.sourceUrl?.let { Text(it, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.primary) }
            }
            if (c.text.isNotEmpty()) {
                SelectionContainer { Text(c.text, Modifier.padding(16.dp, 8.dp), style = MaterialTheme.typography.bodyLarge) }
            }
            val live = c.chunks.filter { it.status == "active" && it.supersededBy == null }
            if (live.isNotEmpty()) {
                SectionHead("Artifacts")
                live.forEach { a ->
                    LinkRow(if (a.named) a.title!! else opening(a.text, 60), a.named) { onArtifact(a.id) }
                }
            }
        }
    }
}
