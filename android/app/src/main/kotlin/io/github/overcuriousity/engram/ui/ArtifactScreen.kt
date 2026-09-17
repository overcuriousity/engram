package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.Node
import io.github.overcuriousity.engram.core.read.Version
import java.time.ZoneId

/**
 * One artifact: its text, where it came from, how it came to exist, and the
 * wordings it has had.
 *
 * This is the only place the artifact route is read, and it is read because a
 * person opened this screen. The server counts that fetch as an open — it feeds
 * what gets primed — so nothing may fetch it on a person's behalf.
 *
 * Lineage and versions are their own reads with their own states: the text
 * does not wait for a tree, and a tree that cannot be fetched says so beneath a
 * text that could.
 */
@Composable
fun ArtifactScreen(engram: Engram, id: String, onCorpus: (String) -> Unit, onArtifact: (String) -> Unit) {
    val state = rememberRead(engram, Api.artifact(id), Decode.artifact)
    val zone = ZoneId.systemDefault()
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        ReadFrame(state) { a ->
            val c = a.chunk
            Column(Modifier.padding(16.dp, 8.dp)) {
                if (c.named) Label(c.title!!, named = true)
                val facts = listOfNotNull(
                    c.provenance.takeIf { it.isNotEmpty() },
                    c.category,
                    dayWords(c.createdAt, System.currentTimeMillis(), zone),
                    "superseded".takeIf { c.supersededBy != null },
                    c.status.takeIf { it != "active" },
                )
                Text(facts.joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = muted())
                if (c.tags.isNotEmpty()) Text(c.tags.joinToString("  ") { "#$it" }, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.primary)
            }
            SelectionContainer { Text(c.text, Modifier.padding(16.dp, 8.dp), style = MaterialTheme.typography.bodyLarge) }
            c.caveats.forEach { Text("⚠ $it", Modifier.padding(16.dp, 2.dp), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.secondary) }
            a.source?.let { s ->
                val lines = c.span?.let { if (it.startLine == it.endLine) "line ${it.startLine} of " else "lines ${it.startLine}–${it.endLine} of " } ?: "from "
                LinkRow(lines + (s.title ?: s.sourceUrl ?: s.origin), named = false) { onCorpus(s.id) }
            }
        }

        val lineage = rememberRead(engram, Api.lineage(id), Decode.lineage)
        ReadFrame(lineage) { l ->
            if (l.roots.isNotEmpty()) {
                SectionHead("Written from")
                l.roots.forEach { Tree(it, 0, zone, onArtifact) }
            }
            if (l.alsoReplaced.isNotEmpty()) {
                SectionHead("Replaced without being merged")
                l.alsoReplaced.forEach { Tree(it, 0, zone, onArtifact) }
            }
            // A tree that quietly stops reads as a whole history.
            if (l.truncated) Text("…and more: the history is longer than is shown", Modifier.padding(16.dp, 4.dp), style = MaterialTheme.typography.labelSmall, color = muted())
        }

        val versions = rememberRead(engram, Api.versions(id), Decode.versions)
        ReadFrame(versions) { page ->
            if (page.items.isNotEmpty()) {
                SectionHead("Earlier wordings")
                page.items.forEach { VersionLine(it, zone) }
            }
        }
    }
}

@Composable
private fun Tree(n: Node, depth: Int, zone: ZoneId, onArtifact: (String) -> Unit) {
    val words = listOfNotNull(
        n.kind.takeIf { it.isNotEmpty() && !n.missing },
        n.createdAt.takeIf { it > 0 }?.let { dayWords(it, System.currentTimeMillis(), zone) },
        "replaced".takeIf { n.replaced },
    ).joinToString(" · ")
    Column(
        Modifier.fillMaxWidth()
            // A source deleted since is named, not dropped — and is not a link.
            .then(if (n.missing) Modifier else Modifier.clickable { onArtifact(n.id) })
            .padding(start = (16 + depth * 16).dp, end = 16.dp, top = 6.dp, bottom = 6.dp),
    ) {
        Label(if (n.missing) "deleted since" else n.label, n.named && !n.missing, maxLines = 1)
        if (words.isNotEmpty()) Text(words, style = MaterialTheme.typography.labelSmall, color = muted())
        n.source?.let { s ->
            val where = when {
                s.startLine == null -> "from ${s.label}"
                s.startLine == s.endLine -> "line ${s.startLine} of ${s.label}"
                else -> "lines ${s.startLine}–${s.endLine} of ${s.label}"
            }
            Text(where, style = MaterialTheme.typography.labelSmall, color = muted())
        }
    }
    n.children.forEach { Tree(it, depth + 1, zone, onArtifact) }
}

@Composable
private fun VersionLine(v: Version, zone: ZoneId) {
    var open by rememberSaveable(v.n) { mutableStateOf(false) }
    Column(Modifier.fillMaxWidth().clickable { open = !open }.padding(16.dp, 8.dp)) {
        Text("${if (open) "▾" else "▸"} version ${v.n} · ${dayWords(v.createdAt, System.currentTimeMillis(), zone)}", style = MaterialTheme.typography.labelMedium, color = muted())
        if (open) {
            SelectionContainer { Text(v.text, Modifier.padding(top = 6.dp), style = MaterialTheme.typography.bodyMedium) }
            v.caveats.forEach { Text("⚠ $it", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.secondary) }
        }
    }
}
