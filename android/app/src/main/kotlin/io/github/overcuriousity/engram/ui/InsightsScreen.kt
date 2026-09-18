package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.clickable
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
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import io.github.overcuriousity.engram.core.read.Machine

/**
 * What this memory is like, and what the base did on its own: the web's
 * Insights, behind Settings for the reason judging is. The measures are
 * aggregates over tables that exist; nothing here embeds or calls a model.
 * Disclosure and nothing else: the base tunes itself, and this page says
 * what it did.
 */
@Composable
fun InsightsScreen(
    engram: Engram,
    onJudging: (Screen) -> Unit,
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
                Text("This page measures what the base holds and lists what needs you. The measures wait on there being something in it.", Modifier.padding(16.dp, 0.dp), style = MaterialTheme.typography.bodySmall, color = muted())
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
            SectionHead("Needs you")
            Line("Duplicate pairs" + (if (rep.morePairs > 0) " · ${rep.morePairs} more waiting" else "")) { onJudging(Screen.Pairs) }
            Line("Gaps") { onJudging(Screen.Gaps) }
            Line("Set aside for you") { onJudging(Screen.Journal) }
            rep.pursuits?.let { (recent, unsatisfied) ->
                if (recent > 0) Text(
                    "$recent run${if (recent != 1L) "s" else ""} of searches went quiet" + (if (unsatisfied > 0) ", of which $unsatisfied went unanswered and ${if (unsatisfied == 1L) "is" else "are"} on the gap list" else "") + ".",
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
