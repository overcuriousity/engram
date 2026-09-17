package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
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
import io.github.overcuriousity.engram.core.outbox.Resolution
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.GapCluster
import io.github.overcuriousity.engram.core.read.GapMember
import io.github.overcuriousity.engram.core.read.Pair
import io.github.overcuriousity.engram.core.read.SetAsideAction
import io.github.overcuriousity.engram.core.read.SetAsideRow
import io.github.overcuriousity.engram.core.read.actionsFor
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch

/*
 * Judging: the decisions a person makes about the base, rather than reads.
 *
 * All of it lives behind Settings. Not a tab, not a section on the screen the
 * app opens on, and with no count anywhere a person has not asked for one. A
 * queue of chores on the home screen is an interface asking to be served, and
 * this is the part of the app meant to shrink: the human in the loop here is
 * one to work towards removing, not one to build a habit around.
 *
 * Every answer goes through the outbox like every other write the device owes.
 * That is what makes a decision on a train a decision, and it is where the undo
 * gets its window for free: until the row is delivered, taking it back is
 * deleting a row.
 *
 * The lists are drawn by composables that know nothing about Engram, so the
 * rules about what a card may say can be checked without a device.
 */

/**
 * What the last opened judging read held. Session-lived, and never fetched
 * for: a count appears on a Settings line only once somebody has opened the
 * screen that fetched it. Nothing here is a badge, and nothing polls.
 */
object JudgeCounts {
    val pairs = MutableStateFlow<Int?>(null)
    val gaps = MutableStateFlow<Int?>(null)
    val setAside = MutableStateFlow<Int?>(null)
}

// ── The pair review ──────────────────────────────────────────────────────────

@Composable
fun PairReviewScreen(engram: Engram, onArtifact: (String) -> Unit) {
    val state = rememberRead(engram, Api.pairs(), Decode.pairs)
    Column(Modifier.fillMaxSize()) {
        Head("Duplicate pairs")
        ReadFrame(state) { page ->
            val cards = remember(page) { cardsOf(page.items) }
            // After the read, never during it: a count is what was fetched.
            LaunchedEffect(cards.size) { JudgeCounts.pairs.value = cards.size }
            PairReview(
                cards = cards,
                onAnswer = { p, a ->
                    when (a) {
                        PairAnswer.KeepA -> engram.outbox.enqueuePairSupersede(p.id, p.a.id)
                        PairAnswer.KeepB -> engram.outbox.enqueuePairSupersede(p.id, p.b.id)
                        PairAnswer.WriteOne -> engram.outbox.enqueuePairSynthesize(p.id)
                        PairAnswer.DiscardBoth -> engram.outbox.enqueuePairDiscard(p.id)
                        PairAnswer.Dismiss -> engram.outbox.enqueuePairDismiss(p.id)
                    }
                },
                onUndo = { engram.outbox.undo(it) },
                onArtifact = onArtifact,
            )
        }
    }
}

/**
 * One card at a time, and what is left to answer above it.
 *
 * A card leaves the deck when it is answered rather than an index moving over
 * it, so a re-read — the held answer, then the server's — cannot put somebody
 * back at the top of a queue they have been working through.
 */
@Composable
fun PairReview(
    cards: List<PairCard>,
    onAnswer: suspend (Pair, PairAnswer) -> String,
    onUndo: suspend (String) -> Boolean,
    onArtifact: (String) -> Unit,
    modifier: Modifier = Modifier.verticalScroll(rememberScrollState()),
) {
    val scope = rememberCoroutineScope()
    val answered = remember(cards) { mutableStateListOf<Long>() }
    var undo by remember(cards) { mutableStateOf<Undoable?>(null) }
    var confirming by remember(cards) { mutableStateOf<PairAnswer?>(null) }

    val deck = cards.filter { it.pair.id !in answered }
    val card = deck.firstOrNull()

    fun answer(c: PairCard, a: PairAnswer) {
        val p = c.pair
        scope.launch {
            val id = onAnswer(p, a)
            answered += p.id
            undo = Undoable(id, p.id, answerWords(a, p))
            confirming = null
        }
    }

    Column(modifier) {
        undo?.let { u ->
            UndoBar(u.words) {
                scope.launch {
                    if (onUndo(u.outboxId)) answered.remove(u.subject)
                    undo = null
                }
            }
        }
        if (card == null) {
            Text(
                if (cards.isEmpty()) "Nothing to answer" else "Answered",
                Modifier.padding(16.dp),
                color = muted(),
            )
            return@Column
        }
        Text(
            "${answered.size + 1} of ${cards.size}" +
                if (card.siblings > 1) " · ${card.siblings} about this artifact" else "",
            Modifier.padding(16.dp, 4.dp),
            style = MaterialTheme.typography.labelMedium,
            color = muted(),
        )
        Side(card.pair.a, onArtifact)
        Column(Modifier.padding(16.dp, 8.dp)) {
            Text(
                pairWords(card.pair).joinToString(" · "),
                style = MaterialTheme.typography.labelSmall,
                color = muted(),
            )
            findingOf(card.pair)?.let {
                Text(it, style = MaterialTheme.typography.bodyMedium, fontWeight = FontWeight.Medium)
            }
        }
        Side(card.pair.b, onArtifact)
        Spacer(Modifier.height(8.dp))
        FlowRow(
            Modifier.fillMaxWidth().padding(12.dp, 4.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            answersFor(card.pair).forEach { a ->
                val keeps = card.pair.keeps
                val proposed = (a == PairAnswer.KeepA && keeps == card.pair.a.id) ||
                    (a == PairAnswer.KeepB && keeps == card.pair.b.id)
                val press = { if (answerCost(a, card.pair) == null) answer(card, a) else confirming = a }
                if (proposed) {
                    Button(onClick = press) { Text(answerWords(a, card.pair)) }
                } else {
                    OutlinedButton(onClick = press) { Text(answerWords(a, card.pair)) }
                }
            }
        }

        confirming?.let { a ->
            AlertDialog(
                onDismissRequest = { confirming = null },
                title = { Text(answerWords(a, card.pair)) },
                text = { Text(answerCost(a, card.pair).orEmpty()) },
                confirmButton = { TextButton(onClick = { answer(card, a) }) { Text("Do it") } },
                dismissButton = { TextButton(onClick = { confirming = null }) { Text("Keep both") } },
            )
        }
    }
}

@Composable
private fun Side(side: io.github.overcuriousity.engram.core.read.PairSide, onArtifact: (String) -> Unit) {
    Surface(color = MaterialTheme.colorScheme.surfaceContainer, modifier = Modifier.fillMaxWidth().padding(12.dp, 4.dp)) {
        Column(Modifier.padding(12.dp)) {
            // A side with no name is labelled by its own opening, so a label
            // above the excerpt would be the same words twice. The text is
            // what there is; it is not dressed as a name it does not have.
            if (side.named) Label(side.label, named = true)
            Text(side.excerpt, style = MaterialTheme.typography.bodyMedium, color = muted())
            TextButton(onClick = { onArtifact(side.id) }, contentPadding = androidx.compose.foundation.layout.PaddingValues(0.dp)) {
                Text("Open", style = MaterialTheme.typography.labelMedium)
            }
        }
    }
}

// ── Gaps ─────────────────────────────────────────────────────────────────────

@Composable
fun GapsScreen(engram: Engram) {
    val state = rememberRead(engram, Api.gaps(), Decode.gaps)
    Column(Modifier.fillMaxSize()) {
        Head("Gaps")
        ReadFrame(state) { page ->
            val n = page.items.sumOf { it.members.size }
            LaunchedEffect(n) { JudgeCounts.gaps.value = n }
            Gaps(
                clusters = page.items,
                onDismiss = { m -> engram.outbox.enqueueGapDismiss(m.kind, m.id) },
                onForget = { ms -> engram.outbox.enqueueGapForget(ms) },
            )
        }
    }
}

@Composable
fun Gaps(
    clusters: List<GapCluster>,
    onDismiss: suspend (GapMember) -> String,
    onForget: suspend (List<GapMember>) -> String,
    modifier: Modifier = Modifier.verticalScroll(rememberScrollState()),
) {
    val scope = rememberCoroutineScope()
    val gone = remember(clusters) { mutableStateListOf<String>() }
    var forgetting by remember(clusters) { mutableStateOf<GapCluster?>(null) }

    Column(modifier) {
        if (clusters.isEmpty()) {
            Text("Nothing unanswered", Modifier.padding(16.dp), color = muted())
            return@Column
        }
        clusters.forEach { c ->
            val left = c.members.filter { it.id !in gone }
            if (left.isEmpty()) return@forEach
            Column(Modifier.padding(16.dp, 8.dp)) {
                // A name a model gave is a reading, and one taken from the
                // shared wording is not. Which it was is said, not implied.
                Label(c.label, named = false)
                Text(labelledWords(c.labelledBy), style = MaterialTheme.typography.labelSmall, color = muted())
            }
            left.forEach { m ->
                Row(
                    Modifier.fillMaxWidth().padding(start = 24.dp, end = 4.dp),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(m.text, Modifier.weight(1f), style = MaterialTheme.typography.bodyMedium)
                    TextButton(onClick = { scope.launch { onDismiss(m); gone += m.id } }) { Text("Dismiss") }
                }
            }
            if (left.size > 1) {
                TextButton(onClick = { forgetting = c }, modifier = Modifier.padding(start = 16.dp)) { Text("Forget all") }
            }
            HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
        }
    }

    forgetting?.let { c ->
        val left = c.members.filter { it.id !in gone }
        AlertDialog(
            onDismissRequest = { forgetting = null },
            title = { Text("Forget all") },
            text = { Text("${left.size} questions, and nothing is asked about them again.") },
            confirmButton = {
                TextButton(onClick = {
                    scope.launch { onForget(left); gone += left.map { it.id }; forgetting = null }
                }) { Text("Forget") }
            },
            dismissButton = { TextButton(onClick = { forgetting = null }) { Text("Keep") } },
        )
    }
}

// ── While you were away ──────────────────────────────────────────────────────

@Composable
fun JournalScreen(engram: Engram, onArtifact: (String) -> Unit, onCorpus: (String) -> Unit) {
    val state = rememberRead(engram, Api.setAside(), Decode.setAside)
    Column(Modifier.fillMaxSize()) {
        Head("While you were away")
        ReadFrame(state) { s ->
            LaunchedEffect(s.items.size) { JudgeCounts.setAside.value = s.items.size }
            Journal(
                rows = s.items,
                capped = s.capped,
                onAction = { row, a ->
                    when (a) {
                        SetAsideAction.Verify -> engram.outbox.enqueueArtifactOp(row.subjectId, ArtifactOp.verify)
                        SetAsideAction.Deprecate -> engram.outbox.enqueueArtifactOp(row.subjectId, ArtifactOp.deprecate)
                        SetAsideAction.Reactivate -> engram.outbox.enqueueArtifactOp(row.subjectId, ArtifactOp.reactivate)
                        SetAsideAction.UndoMerge -> engram.outbox.enqueueMergeUndo(row.subjectId)
                        SetAsideAction.ResolveReplace -> engram.outbox.enqueueCorpusResolve(row.subjectId, Resolution.replace)
                        SetAsideAction.ResolveKeepBoth -> engram.outbox.enqueueCorpusResolve(row.subjectId, Resolution.keep_both)
                        SetAsideAction.ResolveDiscard -> engram.outbox.enqueueCorpusResolve(row.subjectId, Resolution.discard)
                    }
                },
                onUndo = { engram.outbox.undo(it) },
                onOpen = { row -> row.artifactId?.let(onArtifact) ?: onCorpus(row.subjectId) },
            )
        }
    }
}

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
    val answered = remember(rows) { mutableStateListOf<String>() }
    var undo by remember(rows) { mutableStateOf<Undoable?>(null) }

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
            Text("Nothing set aside", Modifier.padding(16.dp), color = muted())
            return@Column
        }
        rows.filter { it.subjectId !in answered }.forEach { row ->
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
                                answered += row.subjectId
                                undo = Undoable(id, row.subjectId, setAsideWords(a))
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

// ── Shared ───────────────────────────────────────────────────────────────────

/** An answer that is on its way and can still be taken back. */
data class Undoable(val outboxId: String, val subject: Any, val words: String)

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

@Composable
private fun Head(title: String) {
    Text(title, Modifier.padding(16.dp, 8.dp), style = MaterialTheme.typography.titleLarge)
}
