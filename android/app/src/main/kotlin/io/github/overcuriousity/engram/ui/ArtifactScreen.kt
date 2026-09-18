package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.outbox.ArtifactOp
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Chunk
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.Node
import io.github.overcuriousity.engram.core.read.RelatedRow
import io.github.overcuriousity.engram.core.read.SourceSlice
import io.github.overcuriousity.engram.core.read.Version
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.launch
import java.time.ZoneId

/**
 * One artifact, as the web's pane has it: its text drawn as text rather than
 * as markdown syntax, the decisions about it, the lines it was drawn from,
 * what it resembles and what it has been needed alongside, where it came from,
 * and the wordings it has had.
 *
 * This is the only place the artifact route is read, and it is read because a
 * person opened this screen. The server counts that fetch as an open — it feeds
 * what gets primed — so nothing may fetch it on a person's behalf.
 *
 * Every other section is its own read with its own state: the text does not
 * wait for a neighbour list, and a source that cannot be fetched says so
 * beneath a text that could.
 */
@Composable
fun ArtifactScreen(
    engram: Engram,
    id: String,
    onCorpus: (String, Long?, Long?) -> Unit,
    onArtifact: (String) -> Unit,
    /** The search this was opened from, where it was: the open is attributed to it and the bar is drawn. */
    event: String? = null,
) {
    val state = rememberRead(engram, Api.artifact(id, event), Decode.artifact)
    val zone = ZoneId.systemDefault()
    val scope = rememberCoroutineScope()
    val doors = rememberRead(engram, Api.status(), Decode.status).read.value
    // How long this was on screen, told when it leaves: the dwell the web's
    // pane reports. Fire and forget, as the web's is — and told from a scope
    // that outlives the screen, because this one's is cancelled by the very
    // departure that is being reported.
    DisposableEffect(id) {
        val opened = System.currentTimeMillis()
        onDispose {
            val secs = (System.currentTimeMillis() - opened) / 1000
            if (secs > 0) engram.telling.launch { engram.reader.tell(Api.dwell(id), Api.json("secs" to secs)) }
        }
    }
    var editing by remember { mutableStateOf<String?>(null) }
    var reviewed by remember { mutableStateOf(false) }
    val dismissed = remember { mutableStateListOf<String>() }
    // What was decided here, before the server has been told. The read above
    // will not show it until the row is delivered and the screen is opened
    // again, and a person who pressed Hide must not be shown an artifact that
    // still claims to be in results.
    var decided by remember { mutableStateOf<Decision?>(null) }
    fun decide(d: Decision, write: suspend () -> String) {
        scope.launch {
            decided = d.copy(row = write())
            Sync.kick(engram.app)
        }
    }
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

            // Verification failures, and the judgement that clears them: the
            // operator looked at the chunk beside its source lines and decided
            // the warning was noise.
            if (c.flags.isNotEmpty() && !reviewed) {
                Flag(c.flags.joinToString(", "), c.flagDetail ?: "") {
                    TextButton(onClick = {
                        reviewed = true
                        scope.launch { engram.outbox.enqueueCall("Artifact · marked reviewed", "POST", Api.reviewed(c.id)); Sync.kick(engram.app) }
                    }) { Text("Mark reviewed") }
                }
            }
            Decisions(
                chunk = c,
                decided = decided,
                onVerify = { decide(Decision(ArtifactAnswer.Verify)) { engram.outbox.enqueueArtifactOp(c.id, ArtifactOp.verify) } },
                onHide = { decide(Decision(ArtifactAnswer.Hide)) { engram.outbox.enqueueArtifactOp(c.id, ArtifactOp.deprecate) } },
                onReactivate = {
                    // The server's own unsupersede for a superseded one; a
                    // deprecated one has no winner and reactivates.
                    val op = if (c.supersededBy != null) ArtifactOp.unsupersede else ArtifactOp.reactivate
                    decide(Decision(ArtifactAnswer.Reactivate)) { engram.outbox.enqueueArtifactOp(c.id, op) }
                },
                onDelete = { decide(Decision(ArtifactAnswer.Delete)) { engram.outbox.enqueueArtifactDelete(c.id) } },
                onUndo = { row -> scope.launch { if (engram.outbox.undo(row)) decided = null } },
                onWinner = onArtifact,
            )

            // The same box the corpus page edits in, and the same route: the
            // vector describes wording that no longer exists, so a save
            // re-embeds. Pressed where the server is and answered in words.
            val draft = editing
            if (draft != null) {
                var saidNo by remember { mutableStateOf<String?>(null) }
                Column(Modifier.padding(16.dp, 8.dp)) {
                    OutlinedTextField(value = draft, onValueChange = { editing = it }, modifier = Modifier.fillMaxWidth(), minLines = 6)
                    saidNo?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error) }
                    Row(Modifier.padding(top = 8.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        Button(onClick = {
                            scope.launch {
                                val r = engram.reader.call("PATCH", Api.artifactEdit(c.id), Api.json("text" to draft), Decode.nothing)
                                if (r.value != null) { editing = null; state.retry() } else saidNo = r.error ?: "Server unreachable"
                            }
                        }, enabled = draft.isNotBlank()) { Text("Save and re-embed") }
                        TextButton(onClick = { editing = null }) { Text("Cancel") }
                    }
                }
            } else {
                // A passage is kept as the document wrote it; everything else was
                // written as markdown by a model. The same rule as `artifact_html`.
                if (c.provenance == "passage") Verbatim(c.text, Modifier.padding(16.dp, 8.dp))
                else Markdown(c.text, Modifier.padding(16.dp, 8.dp))
                Row(Modifier.padding(horizontal = 8.dp)) {
                    TextButton(onClick = { editing = c.text }) { Text("Edit") }
                }
            }
            // Under the text, once it has been read: was it the one? Only where
            // this was opened from a list and the search was recorded.
            val ev = a.searchEvent
            if (ev != null && doors?.learn != false) SearchVerdictBar(engram, ev, c.id)
            // What the base found when this arrived, what has asked for it,
            // and the way back to the last wording where the live one is
            // condensed. Its own read: none of it is the artifact.
            val about = rememberRead(engram, Api.about(id), Decode.about)
            about.read.value?.let { ab ->
                Column(Modifier.padding(16.dp, 4.dp)) {
                    ab.tag?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = muted()) }
                    val badges = listOfNotNull(c.category, ab.dueIn?.let { "due $it" })
                    if (badges.isNotEmpty()) Row(horizontalArrangement = Arrangement.spacedBy(6.dp), modifier = Modifier.padding(top = 4.dp)) {
                        c.category?.let { Badge(it, MaterialTheme.colorScheme.primary) }
                        ab.dueIn?.let { Badge("due $it", due()) }
                    }
                    if (ab.probes.isNotEmpty()) Fold("what has asked for this (${ab.probes.size})") {
                        ab.probes.forEach { Text(it, style = MaterialTheme.typography.labelMedium, color = muted()) }
                    }
                    ab.condensed?.let { action ->
                        TextButton(onClick = {
                            scope.launch { engram.outbox.enqueueCall("Condensation · undone", "POST", Api.condensationUndo(action)); Sync.kick(engram.app); state.retry() }
                        }) { Text("Restore the last version") }
                    }
                }
            }
            if (c.cues.isNotEmpty()) {
                // Not "a model guessed you would want this" but "this was written
                // because these things were asked and the base had no answer".
                SectionHead("Written because these were asked")
                c.cues.forEach { Text(it, Modifier.padding(16.dp, 2.dp), style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant) }
            }
            if (c.caveats.isNotEmpty()) {
                Column(Modifier.padding(16.dp, 8.dp)) {
                    Text("Before you rely on this", style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.secondary)
                    c.caveats.forEach { Text("· $it", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.secondary) }
                }
            }
        }

        // The pivots: onward, what this resembles, what it is needed alongside.
        val related = rememberRead(engram, Api.related(id), Decode.related)
        ReadFrame(related) { r ->
            r.continuesAt?.let { next ->
                LinkRow("continues in the next passage →", named = false) { onArtifact(next) }
            }
            if (r.related.isNotEmpty()) {
                SectionHead("Related")
                r.related.forEach { RelatedLine(it, onArtifact) }
            }
            val links = r.seenTogether.filter { it.id !in dismissed }
            if (links.isNotEmpty()) {
                SectionHead("Seen together")
                links.forEach { l ->
                    Row(verticalAlignment = androidx.compose.ui.Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) { RelatedLine(l, onArtifact) }
                        // The operator saying this pair does not belong together. Final for that pair.
                        TextButton(onClick = {
                            dismissed += l.id
                            scope.launch { engram.outbox.enqueueCall("Link · not related", "POST", Api.dismissLink(id, l.id)); Sync.kick(engram.app) }
                        }) { Text("Not related", style = MaterialTheme.typography.labelSmall) }
                    }
                }
            }
        }

        // The lines this was drawn from, with a little context either side.
        // A merge has none and the lineage below is what says where it came from.
        val source = rememberRead(engram, Api.source(id), Decode.source)
        ReadFrame(source) { s ->
            val span = state.read.value?.chunk?.span
            if (s.corpusId != null) SourceLines(s, state.read.value?.source?.let { it.title ?: it.sourceUrl ?: it.origin }) { onCorpus(it, span?.startLine, span?.endLine) }
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

// ── The decisions ────────────────────────────────────────────────────────

/** An answer this screen can give about an artifact. Each is a route; delete is the one with no undo. */
enum class ArtifactAnswer { Verify, Hide, Reactivate, Delete }

/** One made here, and the outbox row it became — the handle Undo takes it back by while it is still queued. */
data class Decision(val answer: ArtifactAnswer, val row: String? = null)

/**
 * Which answers an artifact admits, from what the server said about it and
 * nothing else — the rule `_artifact_detail.html` follows. Verify and hide
 * only apply to one that is in results; the way back is offered to one that
 * is not; delete is offered whatever the status, because "get rid of this" is
 * a decision that does not depend on whether it is currently hidden.
 */
fun answersFor(status: String, supersededBy: String?): List<ArtifactAnswer> = when {
    supersededBy != null || status == "deprecated" -> listOf(ArtifactAnswer.Reactivate, ArtifactAnswer.Delete)
    else -> listOf(ArtifactAnswer.Verify, ArtifactAnswer.Hide, ArtifactAnswer.Delete)
}

/**
 * The flag over a hidden artifact and the row of answers under it. Words, not
 * icons: what "Hide" hides from, and that the artifact survives it, lived in a
 * tooltip on the web, and this screen has no hover. Delete asks first, and
 * says what the asking is for: hiding can be undone and this cannot.
 */
@Composable
fun Decisions(
    chunk: Chunk,
    decided: Decision?,
    onVerify: () -> Unit,
    onHide: () -> Unit,
    onReactivate: () -> Unit,
    onDelete: () -> Unit,
    onUndo: (String) -> Unit,
    onWinner: (String) -> Unit,
) {
    var confirm by rememberSaveable { mutableStateOf(false) }
    if (decided != null) {
        val words = when (decided.answer) {
            ArtifactAnswer.Verify -> "Marked still accurate"
            ArtifactAnswer.Hide -> "Hidden from results · the artifact is kept"
            ArtifactAnswer.Reactivate -> "Back in results"
            ArtifactAnswer.Delete -> "Deleted · gone from both stores once delivered"
        }
        Flag(words, if (decided.answer == ArtifactAnswer.Delete) "Nothing can put it back after that." else "Delivered when the server can be reached.") {
            decided.row?.let { row -> TextButton(onClick = { onUndo(row) }) { Text("Undo") } }
        }
        return
    }
    chunk.supersededBy?.let { winner ->
        Flag("Hidden from results", "Something near-identical and newer was kept instead.") {
            TextButton(onClick = { onWinner(winner) }) { Text("The one kept") }
        }
    }
    if (chunk.supersededBy == null && chunk.status == "deprecated") {
        Flag("Hidden as stale", "Search will not return it and Ask will not read from it; it is still stored.")
    }
    val answers = answersFor(chunk.status, chunk.supersededBy)
    Row(Modifier.fillMaxWidth().padding(horizontal = 8.dp), horizontalArrangement = Arrangement.spacedBy(0.dp)) {
        if (ArtifactAnswer.Verify in answers) TextButton(onClick = onVerify) { Text("Still accurate") }
        if (ArtifactAnswer.Hide in answers) TextButton(onClick = onHide) { Text("Hide from results") }
        if (ArtifactAnswer.Reactivate in answers) TextButton(onClick = onReactivate) { Text(if (chunk.supersededBy != null) "Put it back" else "Reactivate") }
        Spacer(Modifier.weight(1f))
        // At the far end, so the one control that cannot be undone is never
        // flush against two that can.
        TextButton(onClick = { confirm = true }, colors = ButtonDefaults.textButtonColors(contentColor = MaterialTheme.colorScheme.error)) { Text("Delete") }
    }
    if (confirm) AlertDialog(
        onDismissRequest = { confirm = false },
        title = { Text("Delete this artifact for good?") },
        text = { Text("It goes from both stores, anything written from it loses it as a source, and none of that can be undone. Hide keeps the artifact and only takes it out of results.") },
        confirmButton = {
            TextButton(onClick = { confirm = false; onDelete() }, colors = ButtonDefaults.textButtonColors(contentColor = MaterialTheme.colorScheme.error)) { Text("Delete") }
        },
        dismissButton = { TextButton(onClick = { confirm = false }) { Text("Keep it") } },
    )
}

/** A disclosure: closed by default, a line that opens. */
@Composable
fun Fold(summary: String, content: @Composable () -> Unit) {
    var open by rememberSaveable(summary) { mutableStateOf(false) }
    Column {
        Text("${if (open) "▾" else "▸"} $summary", Modifier.clickable { open = !open }.padding(vertical = 6.dp), style = MaterialTheme.typography.labelMedium, color = muted())
        if (open) content()
    }
}

@Composable
private fun Flag(head: String, body: String, actions: @Composable () -> Unit = {}) {
    Surface(
        color = MaterialTheme.colorScheme.secondary.copy(alpha = 0.10f),
        shape = MaterialTheme.shapes.medium,
        modifier = Modifier.fillMaxWidth().padding(16.dp, 6.dp),
    ) {
        Column(Modifier.padding(12.dp, 8.dp)) {
            Text(head, style = MaterialTheme.typography.labelLarge)
            Text(body, style = MaterialTheme.typography.bodySmall, color = muted())
            Row { actions() }
        }
    }
}

// ── The pivots and the source ────────────────────────────────────────────

@Composable
private fun RelatedLine(r: RelatedRow, onArtifact: (String) -> Unit) {
    Column(Modifier.fillMaxWidth().clickable { onArtifact(r.id) }.padding(16.dp, 8.dp)) {
        if (r.named) Label(r.label, named = true, maxLines = 1)
        r.why?.let { Text(it, style = MaterialTheme.typography.labelSmall, color = muted()) }
        val under = if (r.corpusTitle != null) "${r.snippet} · ${r.corpusTitle}" else r.snippet
        Text(under, maxLines = 2, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}

/**
 * The source column of the web pane: the lines, numbered as the document
 * numbers them, the ones this artifact claims set apart from the context
 * either side. One statement of which lines these are, and it is the link.
 */
@Composable
fun SourceLines(s: SourceSlice, documentName: String?, onCorpus: (String) -> Unit) {
    val corpus = s.corpusId ?: return
    SectionHead("Source")
    LinkRow(listOfNotNull(s.label, documentName).joinToString(" of "), named = false) { onCorpus(corpus) }
    if (s.lines.isEmpty()) return
    SelectionContainer {
        Column(Modifier.fillMaxWidth().padding(16.dp, 4.dp)) {
            s.lines.forEach { l ->
                Row(
                    Modifier.fillMaxWidth()
                        .then(if (l.inSpan) Modifier.background(MaterialTheme.colorScheme.primaryContainer) else Modifier)
                        .padding(4.dp, 2.dp),
                ) {
                    Text(l.number.toString(), Modifier.width(36.dp), style = MaterialTheme.typography.labelMedium, color = muted())
                    Text(
                        l.text.ifEmpty { " " },
                        style = MaterialTheme.typography.bodySmall.copy(fontFamily = Mono),
                        color = if (l.inSpan) MaterialTheme.colorScheme.onBackground else MaterialTheme.colorScheme.onSurfaceVariant,
                        fontWeight = if (l.inSpan) FontWeight.Medium else FontWeight.Normal,
                    )
                }
            }
        }
    }
}

// ── Lineage and versions ─────────────────────────────────────────────────

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
            Markdown(v.text, Modifier.padding(top = 6.dp), style = MaterialTheme.typography.bodyMedium)
            v.caveats.forEach { Text("⚠ $it", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.secondary) }
        }
    }
}
