package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.read.Api
import io.github.overcuriousity.engram.core.read.Decode
import kotlinx.coroutines.launch

/*
 * The two bars the web judges with: *was this what you were looking for?*
 * under a result opened from a search, and *was this right?* under an answer.
 * Each is pressed where the server is and answered in its words — a verdict
 * is what buys the next measurement, and a bar that could not say whether it
 * was recorded would be a bar nobody could trust.
 */

/** What the search bar shows: the buttons, or what was said and its undo. */
data class SearchVerdict(val state: String = "", val already: Boolean = false, val error: String? = null)

/** The bar as drawn, apart from the server, so the rule about what each state offers can be checked without one. */
@Composable
fun SearchVerdictBar(v: SearchVerdict, onVerdict: (String) -> Unit) {
    FlowRow(Modifier.fillMaxWidth().padding(horizontal = 8.dp), horizontalArrangement = Arrangement.spacedBy(0.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
        when {
            v.error != null -> Text(v.error, Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
            v.already -> Text("nothing to record — that search was already judged.", Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = muted())
            v.state.isEmpty() -> {
                Text("Was this what you were looking for?", Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                TextButton(onClick = { onVerdict("hit") }) { Text("Yes") }
                TextButton(onClick = { onVerdict("no") }) { Text("No") }
                TextButton(onClick = { onVerdict("skip") }) { Text("Not sure") }
            }
            // No undo: a skip is not a verdict, so there is nothing to take back.
            v.state == "skip" -> Text("left unanswered", Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = muted())
            else -> {
                Text(if (v.state == "hit") "yes, this was it" else "not this one", Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                TextButton(onClick = { onVerdict("none") }) { Text("Undo") }
            }
        }
    }
}

/** The bar under an opened result, wired to the server. Only where the open was attributed to a search. */
@Composable
fun SearchVerdictBar(engram: Engram, event: String, artifactId: String) {
    val scope = rememberCoroutineScope()
    var v by remember(event, artifactId) { mutableStateOf(SearchVerdict()) }
    SearchVerdictBar(v) { verdict ->
        scope.launch {
            val r = engram.reader.call("POST", Api.searchVerdict(event), Api.json("verdict" to verdict, "artifact_id" to artifactId), Decode.verdict)
            v = r.value?.let { SearchVerdict(it.state, it.already) } ?: SearchVerdict(error = r.error ?: "Server unreachable")
        }
    }
}

/** What the ask bar shows: unjudged, or judged as the server words it. */
data class AskVerdict(val verdict: String? = null, val error: String? = null)

@Composable
fun AskVerdictBar(v: AskVerdict, onVerdict: (String) -> Unit) {
    FlowRow(Modifier.fillMaxWidth().padding(horizontal = 8.dp), horizontalArrangement = Arrangement.spacedBy(0.dp), verticalArrangement = Arrangement.spacedBy(0.dp)) {
        when {
            v.error != null -> Text(v.error, Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
            v.verdict == null -> {
                Text("Was this right?", Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                TextButton(onClick = { onVerdict("right") }) { Text("Right") }
                TextButton(onClick = { onVerdict("wrong") }) { Text("Wrong") }
                TextButton(onClick = { onVerdict("nothing_here") }) { Text("Nothing here") }
            }
            else -> {
                Text("judged ${v.verdict}", Modifier.padding(8.dp, 12.dp), style = MaterialTheme.typography.bodySmall, color = muted())
                TextButton(onClick = { onVerdict("none") }) { Text("Undo") }
            }
        }
    }
}
