package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
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
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.outbox.ArtifactOp
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.Machine
import io.github.overcuriousity.engram.core.read.SetAsideAction
import io.github.overcuriousity.engram.core.read.SetAsideRow
import io.github.overcuriousity.engram.core.read.actionsFor
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch

/**
 * What this memory is like, and what the base did on its own: the web's
 * Insights, behind Settings rather than on the screen the app opens on. The
 * measures are aggregates over tables that exist; nothing here embeds or calls
 * a model. Disclosure and nothing else: the base tunes and curates itself, and
 * this page says what it did.
 */
@Composable
fun InsightsScreen(
    engram: Engram,
    onJournal: () -> Unit,
    onArtifact: (String) -> Unit,
    onCorpus: (String) -> Unit,
    onLibrary: () -> Unit,
) {
    val insights = rememberRead(engram, Api.insights(), Decode.insights)
    val report = rememberRead(engram, Api.report(), Decode.report)
    val machine = rememberRead(engram, Api.machine(), Decode.machine)
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        Text("Insights", Modifier.padding(16.dp, 12.dp), style = MaterialTheme.typography.titleLarge)
        ReadFrame(insights) { i ->
            if (i.held.corpora == 0L) {
                Text("Nothing is held yet", Modifier.padding(16.dp, 8.dp), style = MaterialTheme.typography.titleMedium)
                Text("This page measures what the base holds and says what it did on its own. The measures wait on there being something in it.", Modifier.padding(16.dp, 0.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                return@ReadFrame
            }
            SectionHead("What this memory is like")
            Measure("Held", "${i.held.artifacts}", "artifacts, from ${i.held.corpora} source${if (i.held.corpora != 1L) "s" else ""}") {
                Text("${i.held.synthesized} written by a model · ${i.held.segments} segments — slices the model reads", style = MaterialTheme.typography.bodySmall, color = muted())
            }
            Measure("Use", null, "activation above the capture baseline, decayed") {
                i.used.forEach { b -> Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) { Text(b.label, style = MaterialTheme.typography.bodySmall); Text(b.count.toString(), style = MaterialTheme.typography.labelMedium) } }
            }
            val r = i.retrieval
            Measure("Retrieval", null, when {
                r == null -> "not recording searches, so there is nothing to measure"
                r.judged == 0L -> "nothing judged yet — answer *Was this what you were looking for?* under a result"
                else -> "from ${r.judged} judged search${if (r.judged != 1L) "es" else ""}"
            }) {
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) { Text("recall@10 — right result in the top 10", style = MaterialTheme.typography.bodySmall); Text(r?.recallAt10?.let { "%.2f".format(it) } ?: "—", style = MaterialTheme.typography.labelMedium) }
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) { Text("MRR — how high it ranked", style = MaterialTheme.typography.bodySmall); Text(r?.mrr?.let { "%.2f".format(it) } ?: "—", style = MaterialTheme.typography.labelMedium) }
            }
        }
        ReadFrame(report) { rep ->
            rep.sleep?.let { s ->
                SectionHead("Last night")
                if (s.runs.isEmpty()) Text("The base has not slept yet: it sleeps after ${s.idleMins} quiet minutes, on the retention sweep's tick.", Modifier.padding(16.dp, 0.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                s.runs.forEach { Text("· $it", Modifier.padding(16.dp, 2.dp), style = MaterialTheme.typography.bodyMedium) }
                if (s.unrehearsedCount > 0) Column(Modifier.padding(16.dp, 0.dp)) {
                    Fold("unrehearsed (${s.unrehearsedCount}) — nothing has asked for these") {
                        s.unrehearsed.forEach { u -> LinkRow(u.getOrNull(1).orEmpty(), named = true) { u.getOrNull(0)?.let(onArtifact) } }
                    }
                }
            }
            rep.evolve?.let { e ->
                SectionHead("Ranking")
                e.suspended?.let { Text("Not moving on its own. $it", Modifier.padding(16.dp, 2.dp), style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.Medium) }
                Text(e.mode, Modifier.padding(16.dp, 2.dp), style = MaterialTheme.typography.bodyMedium)
                Text(e.live, Modifier.padding(16.dp, 2.dp), style = MaterialTheme.typography.bodyMedium)
                Column(Modifier.padding(16.dp, 0.dp)) {
                    Fold("its parameters") { Text(e.params, style = MaterialTheme.typography.labelMedium, color = muted()) }
                    Text(e.standing, style = MaterialTheme.typography.bodySmall, color = muted())
                    Text(e.rehearsed, style = MaterialTheme.typography.bodySmall, color = muted())
                    if (e.history.isNotEmpty()) Fold("generations (${e.history.size})") { e.history.forEach { Text(it, style = MaterialTheme.typography.labelMedium, color = muted()) } }
                    e.rules?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = muted()) }
                    if (e.actions.isNotEmpty()) Fold("what the base did to the corpus (${e.actions.size})") { e.actions.forEach { Text(it, style = MaterialTheme.typography.labelMedium, color = muted()) } }
                }
            }
            // There were lines here to the duplicate pairs and the gaps. The base
            // answers both itself now, so what is left is the journal of what
            // it did, each row with the answer that takes it back.
            SectionHead("What the base did")
            Line("What it merged, wrote, hid and buried") { onJournal() }
            // A run that ended unanswered is a hole the base keeps working on by
            // itself; there is no list of them for anybody to answer.
            rep.pursuits?.let { (recent, unsatisfied) ->
                if (recent > 0) Text(
                    "$recent run${if (recent != 1L) "s" else ""} of searches went quiet" + (if (unsatisfied > 0) ", of which $unsatisfied went unanswered" else "") + ".",
                    Modifier.padding(16.dp, 4.dp), style = MaterialTheme.typography.bodySmall, color = muted(),
                )
            }
        }
        DueBand(engram, onArtifact, head = true)
        SectionHead("Recent")
        Line("Everything captured, newest first") { onLibrary() }
        ReadFrame(machine) { m -> MachineView(m) }
    }
}

@Composable
private fun Measure(label: String, figure: String?, sub: String, content: @Composable () -> Unit) {
    Column(Modifier.padding(16.dp, 6.dp)) {
        Text(label, style = MaterialTheme.typography.labelSmall, color = muted())
        figure?.let { Text(it, style = MaterialTheme.typography.headlineMedium.copy(fontFamily = Mono)) }
        Text(sub, style = MaterialTheme.typography.bodySmall, color = muted())
        content()
    }
}

@Composable
private fun Line(text: String, onClick: () -> Unit) {
    Row(Modifier.fillMaxWidth().clickable(onClick = onClick).padding(16.dp, 10.dp), verticalAlignment = Alignment.CenterVertically) {
        Text(text, style = MaterialTheme.typography.bodyLarge)
    }
}

/** What the machine is doing: open, and said on the summary, when something in it is going wrong. */
@Composable
fun MachineView(m: Machine) {
    val wrong = m.lastDayFailures > 0 || m.retrying.isNotEmpty()
    Column(Modifier.padding(16.dp, 8.dp)) {
        Fold("What the machine is doing" + (if (m.lastDayFailures > 0) " · ${m.lastDayFailures} failed" else "") + (if (m.retrying.isNotEmpty()) " · ${m.retrying.size} retrying" else "")) {
            val jobs = if (m.jobs.isEmpty()) "No jobs queued." else m.jobs.joinToString(", ") { "${it.getOrNull(1)?.content} jobs ${it.getOrNull(0)?.content}" } + "."
            Text("${m.artifacts} artifacts, ${m.vectors} embedded. $jobs" + (m.oldestPendingSecs?.let { " Oldest pending job ${it}s old." } ?: "") + (m.links?.let { " ${it.total} links between artifacts, ${it.related} named, ${it.judgeQueue} waiting on the judge." } ?: ""), style = MaterialTheme.typography.bodySmall, color = muted())
            if (m.lastDay.isNotEmpty() || m.lastDayFailures > 0 || m.sweepHistory.isNotEmpty()) {
                Text("The last day", Modifier.padding(top = 8.dp), style = MaterialTheme.typography.labelLarge)
                Text((if (m.lastDay.isEmpty()) "The sweeps ran and found nothing to do." else m.lastDay.joinToString(", ") { "${it.n} ${it.what}" } + ".") + (if (m.lastDayFailures > 0) " ${m.lastDayFailures} run${if (m.lastDayFailures != 1L) "s" else ""} failed." else ""), style = MaterialTheme.typography.bodySmall, color = muted())
                m.sweepHistory.forEach { r ->
                    Text("${r.`when`} · ${r.stage} · " + (if (r.error.isNotEmpty()) "failed: ${r.error}" else if (r.counts.isEmpty()) "nothing to do" else r.counts.joinToString(", ") { "${it.n} ${it.what}" }) + " · ${r.took}", style = MaterialTheme.typography.labelSmall, color = if (r.error.isNotEmpty()) MaterialTheme.colorScheme.error else muted())
                }
            }
            if (m.offerRates.isNotEmpty()) {
                Text("What was offered", Modifier.padding(top = 8.dp), style = MaterialTheme.typography.labelLarge)
                Text("The last thirty days.", style = MaterialTheme.typography.bodySmall, color = muted())
                m.offerRates.forEach { Text("${it.rung} · shown ${it.shown} · opened ${it.opened}", style = MaterialTheme.typography.labelSmall, color = muted()) }
            }
            if (m.retrying.isNotEmpty()) {
                Text("Retrying", Modifier.padding(top = 8.dp), style = MaterialTheme.typography.labelLarge)
                Text("Work that hit something and is waiting to try again. Nothing here needs you.", style = MaterialTheme.typography.bodySmall, color = muted())
                m.retrying.forEach { Text("${it.stage} · ${it.targetId} · ${it.attempts} attempts · next ${it.due} · ${it.lastError}", style = MaterialTheme.typography.labelSmall, color = muted()) }
            }
            if (!wrong && m.retrying.isEmpty()) Text("Nothing retrying.", style = MaterialTheme.typography.bodySmall, color = muted())
        }
    }
}

// ── What the base did ────────────────────────────────────────────────────────

/*
 * The journal: what the base merged, wrote, hid and buried on its own, each
 * row with the answer that takes it back. Nothing in it is a question. It was
 * one screen of three behind Settings — duplicate pairs and gaps were the
 * others — and is the one left, because the base settles those itself now.
 *
 * Every answer goes through the outbox like every other write the device owes.
 * That is what makes an answer on a train an answer, and it is where the undo
 * gets its window for free: until the row is delivered, taking it back is
 * deleting a row.
 *
 * The list is drawn by a composable that knows nothing about Engram, so what a
 * row may say can be checked without a device.
 */

/**
 * How many rows the last opened journal read held. Session-lived, and never
 * fetched for: a count appears on the Settings line only once somebody has
 * opened the screen that fetched it. Nothing here is a badge, and nothing polls.
 */
internal val journalCount = MutableStateFlow<Int?>(null)

@Composable
fun JournalScreen(engram: Engram, onArtifact: (String) -> Unit, onCorpus: (String) -> Unit) {
    val state = rememberRead(engram, Api.setAside(), Decode.setAside)
    Column(Modifier.fillMaxSize()) {
        Text("What the base did", Modifier.padding(16.dp, 8.dp), style = MaterialTheme.typography.titleLarge)
        ReadFrame(state) { s ->
            LaunchedEffect(s.items.size) { journalCount.value = s.items.size }
            Journal(
                rows = s.items,
                capped = s.capped,
                onAction = { row, a ->
                    when (a) {
                        SetAsideAction.Deprecate -> engram.outbox.enqueueArtifactOp(row.subjectId, ArtifactOp.deprecate)
                        SetAsideAction.Reactivate -> engram.outbox.enqueueArtifactOp(row.subjectId, ArtifactOp.reactivate)
                        SetAsideAction.UndoMerge -> engram.outbox.enqueueMergeUndo(row.subjectId)
                    }
                },
                onUndo = { engram.outbox.undo(it) },
                // A row from a server old enough to list parked captures names
                // a corpus and no artifact; it opens where it can.
                onOpen = { row -> row.artifactId?.let(onArtifact) ?: onCorpus(row.subjectId) },
            )
        }
    }
}

/** What a journal row's button says. The answer is the server's; the words are ours. */
fun setAsideWords(a: SetAsideAction): String = when (a) {
    SetAsideAction.Deprecate -> "Hide"
    SetAsideAction.Reactivate -> "Return to results"
    SetAsideAction.UndoMerge -> "Undo the merge"
}

/**
 * What tells one journal row from another.
 *
 * Not `subject_id` alone: one artifact can be under two kinds at once — one
 * the base wrote and later hid is a `generated` row and a `hidden` one — and
 * they take different answers. Keyed by the subject alone, answering either
 * made both disappear, the second having been enqueued for nothing, and one
 * Undo put both back. Under one `kind` a subject appears once; the server
 * keeps that true.
 */
internal fun keyOf(row: SetAsideRow): String = "${row.kind}\u0000${row.subjectId}"

@Composable
fun Journal(
    rows: List<SetAsideRow>,
    capped: Boolean,
    onAction: suspend (SetAsideRow, SetAsideAction) -> String,
    onUndo: suspend (String) -> Boolean,
    onOpen: (SetAsideRow) -> Unit,
    modifier: Modifier = Modifier.verticalScroll(rememberScrollState()),
) {
    val scope = rememberCoroutineScope()
    // Kept across reads, not keyed on `rows`. A read emits twice — the held
    // answer, then the server's — and the second is a different object, so an
    // answer given during the held one would otherwise be forgotten and the
    // row answerable twice. `keyOf` is stable across reads, which is what
    // this holds.
    val answered = remember { mutableStateListOf<String>() }
    var undo by remember { mutableStateOf<Undoable?>(null) }

    Column(modifier) {
        undo?.let { u ->
            UndoBar(u.words) {
                scope.launch {
                    if (onUndo(u.outboxId)) answered.remove(u.subject)
                    undo = null
                }
            }
        }
        if (rows.isEmpty()) {
            Text("Nothing the base did is waiting to be taken back", Modifier.padding(16.dp), color = muted())
            return@Column
        }
        rows.filter { keyOf(it) !in answered }.forEach { row ->
            Column(Modifier.fillMaxWidth().padding(16.dp, 10.dp)) {
                Label(row.label, row.named)
                if (row.subtitle.isNotEmpty()) {
                    Text(row.subtitle, style = MaterialTheme.typography.labelSmall, color = muted())
                }
                Text(row.why, style = MaterialTheme.typography.bodyMedium, color = muted())
                row.beside.forEach {
                    Text("· ${it.label}", style = MaterialTheme.typography.bodySmall, color = muted())
                }
                row.caveat?.let {
                    Text("⚠ $it", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.secondary)
                }
                FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    TextButton(onClick = { onOpen(row) }) { Text("Open") }
                    // A kind this build has never heard of draws no buttons.
                    // It is still worth showing: what the base did is worth
                    // knowing even where this app cannot answer it.
                    actionsFor(row.kind).forEach { a ->
                        TextButton(onClick = {
                            scope.launch {
                                val id = onAction(row, a)
                                answered += keyOf(row)
                                undo = Undoable(id, keyOf(row), setAsideWords(a))
                            }
                        }) { Text(setAsideWords(a)) }
                    }
                }
            }
            HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
        }
        if (capped) {
            Text("…and more than is listed", Modifier.padding(16.dp, 8.dp), style = MaterialTheme.typography.labelSmall, color = muted())
        }
    }
}

/** An answer that is on its way and can still be taken back. */
data class Undoable(val outboxId: String, val subject: String, val words: String)

/**
 * The window an outbox row gives for free. It stays until the next answer, and
 * takes the row back out of the queue while it is still queued — never a
 * second write undoing the first, which is a different and less honest thing.
 */
@Composable
private fun UndoBar(words: String, onUndo: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, modifier = Modifier.fillMaxWidth()) {
        Row(
            Modifier.padding(start = 16.dp, end = 4.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(words, style = MaterialTheme.typography.bodySmall)
            TextButton(onClick = onUndo) { Text("Undo") }
        }
    }
}
