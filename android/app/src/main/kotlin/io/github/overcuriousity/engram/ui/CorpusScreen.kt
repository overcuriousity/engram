package io.github.overcuriousity.engram.ui

import android.content.Intent
import android.graphics.BitmapFactory
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.core.net.toUri
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Band
import io.github.overcuriousity.engram.core.read.Chunk
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.launch
import java.time.ZoneId

/**
 * One source, as the web's corpus page has it: what it is and what can be done
 * to it, the photo or the document it was, and the text band by band beside
 * what was written from each stretch — where nothing was, a red band saying
 * so, with the offer to read that passage again.
 *
 * Two reads: the corpus, which carries the text and its artifacts, and the
 * page — the bands and what stands above them — which the server cuts from
 * the same inputs the web page is cut from, so the two cannot disagree about
 * what was missed. Every write is an outbox row; delete asks first and leaves
 * the screen, because the thing it showed is gone.
 */
@Composable
fun CorpusScreen(
    engram: Engram,
    id: String,
    onArtifact: (String) -> Unit,
    /** Lines to set apart, from an artifact's source link. */
    highlight: Pair<Long, Long>? = null,
    onGone: () -> Unit = {},
) {
    val state = rememberRead(engram, Api.corpus(id), Decode.corpus)
    val page = rememberRead(engram, Api.bands(id), Decode.bands)
    val scope = rememberCoroutineScope()
    val ctx = LocalContext.current
    val zone = ZoneId.systemDefault()
    var confirm by remember { mutableStateOf<Confirm?>(null) }
    var said by remember { mutableStateOf<String?>(null) }

    fun owe(label: String, method: String, path: String, body: String? = null, then: () -> Unit = {}) {
        scope.launch {
            engram.outbox.enqueueCall(label, method, path, body)
            Sync.kick(engram.app)
            said = "$label · delivered when the server can be reached"
            then()
        }
    }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        ReadFrame(state) { c ->
            val p = page.read.value
            Column(Modifier.padding(16.dp, 8.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Label(c.title ?: opening(c.text, 60).ifEmpty { c.origin }, named = c.title != null, Modifier.weight(1f))
                    if (c.status.isNotEmpty()) Badge(c.status, if (c.status == "failed" || c.status == "parked") MaterialTheme.colorScheme.secondary else muted())
                }
                val from = listOfNotNull(c.origin.takeIf { it.isNotEmpty() }, dayWords(c.createdAt, System.currentTimeMillis(), zone))
                Text(from.joinToString(" · "), style = MaterialTheme.typography.labelSmall, color = muted())
                c.sourceUrl?.let { u ->
                    // The one hop back to where this was read. A link, and only
                    // over http and https: the row is rendered in the person's
                    // authenticated session.
                    val safe = u.startsWith("https://") || u.startsWith("http://")
                    Text(
                        "From $u",
                        Modifier.then(if (safe) Modifier.clickable { ctx.startActivity(Intent(Intent.ACTION_VIEW, u.toUri())) } else Modifier).padding(top = 2.dp),
                        style = MaterialTheme.typography.labelSmall, color = if (safe) MaterialTheme.colorScheme.primary else muted(),
                    )
                }
            }
            // The actions: delete last and at the far end, so the one thing
            // here that cannot be undone is never flush against three that can.
            FlowRow(Modifier.fillMaxWidth().padding(horizontal = 8.dp), horizontalArrangement = Arrangement.spacedBy(0.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
                if (p != null && !p.restored && !p.unread) {
                    TextButton(onClick = { owe("Source · re-segmented", "POST", Api.corpusReprocess(id), Api.json("stage" to "segment")) }) { Text("Re-segment") }
                }
                if (p?.image == true) TextButton(onClick = { confirm = Confirm("Read the photo again?", "The current reading and its artifacts are replaced.") { owe("Photo · re-read", "POST", Api.corpusReprocess(id), Api.json("stage" to "describe")) } }) { Text("Re-read") }
                if (p?.pdf == true) TextButton(onClick = { confirm = Confirm("Read the PDF again?", "The current extraction and its artifacts are replaced.") { owe("PDF · re-extracted", "POST", Api.corpusReprocess(id), Api.json("stage" to "extract")) } }) { Text("Re-extract") }
                TextButton(
                    onClick = { confirm = Confirm("Delete this source and all its artifacts?", "Nothing can put them back.") { owe("Source · deleted", "DELETE", Api.corpusDelete(id)) { onGone() } } },
                    colors = ButtonDefaults.textButtonColors(contentColor = MaterialTheme.colorScheme.error),
                ) { Text("Delete") }
            }
            said?.let { Text(it, Modifier.padding(16.dp, 2.dp), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.tertiary) }

            if (p != null) {
                if (p.image) Picture(engram, id)
                p.note?.let { Text("Note: $it", Modifier.padding(16.dp, 4.dp), style = MaterialTheme.typography.bodyMedium) }
                if (p.meta.isNotEmpty()) MetaTable(p.meta)
                if (p.exif.isNotEmpty()) Column(Modifier.padding(16.dp, 0.dp)) { Fold("All ${p.exif.size} EXIF tags") { MetaTable(p.exif, pad = false) } }
                if (p.restored) {
                    Note("Placeholder source", "This source was never captured here. Its artifacts were restored from the vector store after their rows went missing, and every artifact needs a source to belong to, so this row was written to hold them. The text below is those artifacts joined together — not the original document.")
                }
                p.coverage?.let { Text("$it of this capture's wording survived into an artifact.", Modifier.padding(16.dp, 4.dp), style = MaterialTheme.typography.bodySmall, color = muted()) }
                if (p.promoted.isNotEmpty()) FlowRow(Modifier.padding(horizontal = 16.dp), horizontalArrangement = Arrangement.spacedBy(4.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
                    Text("Promoted:", Modifier.padding(top = 12.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                    p.promoted.forEach { w ->
                        TextButton(onClick = { owe("Promotion · undone", "POST", Api.unpromote(id, w.idx)) }) { Text("lines ${w.from}–${w.to} · Undo", style = MaterialTheme.typography.bodySmall) }
                    }
                }
            }

            val byId = c.chunks.associateBy { it.id }
            when {
                p == null -> {}
                p.bands.isEmpty() -> {
                    SectionHead(if (p.restored) "Restored artifacts" else if (p.image) "Transcription" else "Raw source")
                    when {
                        c.text.isNotBlank() -> Verbatim(c.text, Modifier.padding(16.dp, 4.dp))
                        p.image -> Text("Not read yet — the photo is queued for the vision model.", Modifier.padding(16.dp), color = muted())
                        else -> Text("Nothing was captured here — this source has no text.", Modifier.padding(16.dp), color = muted())
                    }
                }
                else -> {
                    if (p.image) {
                        SectionHead("Transcription")
                        Text("The model's reading of the photo, not the source itself.", Modifier.padding(16.dp, 0.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                    }
                    p.bands.forEach { b -> BandView(engram, id, b, byId, highlight, onArtifact, ::owe) { confirm = it } }
                }
            }
            if (p != null && p.unplaced.isNotEmpty()) {
                SectionHead("Not placed in the source")
                Text(
                    if (p.restored) "Restored from the vector store, so they name no lines of the text above — the text above is these artifacts joined back together."
                    else "These artifacts name no lines of this capture, so they sit beside none of it: written before spans were recorded, or by something other than a window read.",
                    Modifier.padding(16.dp, 0.dp), style = MaterialTheme.typography.bodySmall, color = muted(),
                )
                p.unplaced.mapNotNull { byId[it] }.forEach { ArtifactCard(engram, it, onArtifact) }
            }
            if (p != null && p.writtenFrom.isNotEmpty()) {
                SectionHead("Written from this source")
                Text("Model-written artifacts with a source here. Each lists what it was written from on its own page.", Modifier.padding(16.dp, 0.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                p.writtenFrom.forEach { aid -> byId[aid]?.let { ArtifactCard(engram, it, onArtifact) } ?: LinkRow(aid, named = false) { onArtifact(aid) } }
            }
        }
        Waiting(page)
    }
    confirm?.let { q ->
        AlertDialog(
            onDismissRequest = { confirm = null },
            title = { Text(q.title) },
            text = { Text(q.body) },
            confirmButton = { TextButton(onClick = { confirm = null; q.then() }) { Text("Do it") } },
            dismissButton = { TextButton(onClick = { confirm = null }) { Text("Cancel") } },
        )
    }
}

/** A question asked before something is done, and what doing it is. */
data class Confirm(val title: String, val body: String, val then: () -> Unit)

/** One band: the lines, then what was written from them — or the red head over lines nothing was. */
@Composable
private fun BandView(
    engram: Engram,
    corpusId: String,
    b: Band,
    byId: Map<String, Chunk>,
    highlight: Pair<Long, Long>?,
    onArtifact: (String) -> Unit,
    owe: (String, String, String, String?, () -> Unit) -> Unit,
    ask: (Confirm) -> Unit,
) {
    Column(Modifier.fillMaxWidth().padding(vertical = 6.dp).then(if (b.gap) Modifier.background(MaterialTheme.colorScheme.error.copy(alpha = 0.06f)) else Modifier)) {
        SelectionContainer {
            Column(Modifier.padding(16.dp, 4.dp)) {
                b.lines.forEach { l ->
                    val lit = highlight != null && l.number in highlight.first..highlight.second
                    Row(Modifier.fillMaxWidth().then(if (lit) Modifier.background(MaterialTheme.colorScheme.primaryContainer) else Modifier).padding(4.dp, 1.dp)) {
                        Text(l.number.toString(), Modifier.width(36.dp), style = MaterialTheme.typography.labelMedium, color = muted())
                        Text(l.text.ifEmpty { " " }, style = MaterialTheme.typography.bodySmall.copy(fontFamily = Mono), color = if (l.inSpan || lit) MaterialTheme.colorScheme.onBackground else MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
            }
        }
        if (b.gap) {
            Column(Modifier.padding(16.dp, 4.dp)) {
                Text("lines ${b.from}–${b.to} · nothing was written from these", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.error)
                b.reread?.let { w ->
                    TextButton(onClick = {
                        ask(Confirm("Read this passage again?", "One model call. Nothing already written from this capture is replaced — what comes back is added to it, and anything it repeats is folded by the dedupe sweep.") {
                            owe("Passage · re-read", "POST", Api.corpusReread(corpusId), Api.json("from" to b.from, "to" to b.to)) {}
                        })
                    }) { Text("Read this again — $w") }
                }
            }
        }
        b.echoes.mapNotNull { byId[it] }.forEach { e ->
            Text("↑ ${if (e.named) e.title else opening(e.text, 60)}", Modifier.clickable { onArtifact(e.id) }.padding(16.dp, 2.dp), style = MaterialTheme.typography.labelSmall, color = muted())
        }
        b.artifactIds.mapNotNull { byId[it] }.forEach { ArtifactCard(engram, it, onArtifact) }
    }
}

/** The `_artifact.html` card: the text as text, and the way to the artifact's own page where edit and delete live. */
@Composable
private fun ArtifactCard(engram: Engram, c: Chunk, onArtifact: (String) -> Unit) {
    Surface(color = MaterialTheme.colorScheme.surface, shape = MaterialTheme.shapes.medium, modifier = Modifier.fillMaxWidth().padding(16.dp, 6.dp).clickable { onArtifact(c.id) }) {
        Column(Modifier.padding(14.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (c.named) Label(c.title!!, true, Modifier.weight(1f), maxLines = 1)
                if (c.status != "active" || c.supersededBy != null) Badge(if (c.supersededBy != null) "superseded" else c.status, muted())
            }
            if (c.provenance == "passage") Verbatim(c.text, Modifier.padding(top = 6.dp)) else Markdown(c.text, Modifier.padding(top = 6.dp), style = MaterialTheme.typography.bodyMedium)
        }
    }
}

/** The captured photo, fetched with the phone's credential and drawn at the width of the screen. */
@Composable
private fun Picture(engram: Engram, corpusId: String) {
    var bitmap by remember(corpusId) { mutableStateOf<ImageBitmap?>(null) }
    LaunchedEffect(corpusId) {
        bitmap = engram.picture(corpusId)?.let { runCatching { BitmapFactory.decodeByteArray(it, 0, it.size)?.asImageBitmap() }.getOrNull() }
    }
    bitmap?.let { Image(it, contentDescription = "captured image", Modifier.fillMaxWidth().padding(16.dp, 8.dp), contentScale = ContentScale.FillWidth) }
}

@Composable
private fun MetaTable(rows: List<List<String>>, pad: Boolean = true) {
    Column(Modifier.padding(if (pad) 16.dp else 0.dp, 4.dp)) {
        rows.forEach { r ->
            Row {
                Text(r.getOrNull(0).orEmpty(), Modifier.width(120.dp), style = MaterialTheme.typography.labelSmall, color = muted())
                Text(r.getOrNull(1).orEmpty(), style = MaterialTheme.typography.labelSmall)
            }
        }
    }
}

@Composable
private fun Note(head: String, body: String) {
    Surface(color = MaterialTheme.colorScheme.secondary.copy(alpha = 0.10f), shape = MaterialTheme.shapes.medium, modifier = Modifier.fillMaxWidth().padding(16.dp, 6.dp)) {
        Column(Modifier.padding(12.dp, 8.dp)) {
            Text(head, style = MaterialTheme.typography.labelLarge, fontWeight = FontWeight.Medium)
            Text(body, style = MaterialTheme.typography.bodySmall, color = muted())
        }
    }
}
