package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.github.overcuriousity.engram.core.Engram
import io.github.overcuriousity.engram.core.db.Kind
import io.github.overcuriousity.engram.core.db.State
import io.github.overcuriousity.engram.core.sync.Sync
import kotlinx.coroutines.launch
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive

@Composable
fun QueueScreen(engram: Engram) {
    val rows by engram.outbox.rows.collectAsStateWithLifecycle(emptyList())
    val scope = rememberCoroutineScope()
    val now = System.currentTimeMillis()
    Column(Modifier.fillMaxSize()) {
        Row(
            Modifier.fillMaxWidth().padding(16.dp, 8.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("Queue", style = MaterialTheme.typography.titleLarge)
            TextButton(onClick = { Sync.kick(engram.app) }) { Text("Deliver now") }
        }
        if (rows.isEmpty()) Text("Nothing owed", Modifier.padding(16.dp), color = muted())
        LazyColumn {
            items(rows, key = { it.id }) { row ->
                ListItem(
                    colors = ListItemDefaults.colors(containerColor = MaterialTheme.colorScheme.background),
                    headlineContent = { Text(firstLine(row.kind, row.payload), maxLines = 2) },
                    supportingContent = { Text(rowWords(row, now), style = MaterialTheme.typography.bodySmall, color = muted()) },
                    trailingContent = {
                        if (row.state == State.held) {
                            TextButton(onClick = { scope.launch { engram.outbox.delete(row.id) } }) { Text("Delete") }
                        }
                    },
                )
                HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
            }
        }
    }
}

private fun firstLine(kind: Kind, payload: String): String {
    val p = runCatching { Json.parseToJsonElement(payload).jsonObject }.getOrNull() ?: return kind.name
    return when (kind) {
        Kind.capture_text -> p["text"]?.jsonPrimitive?.contentOrNull?.lineSequence()?.firstOrNull { it.isNotBlank() } ?: "(text)"
        Kind.capture_files -> p["title"]?.jsonPrimitive?.contentOrNull ?: "Files"
        Kind.done -> "Done · ${p["moment"]?.jsonPrimitive?.contentOrNull}"
        Kind.snooze -> "Snoozed · ${p["moment"]?.jsonPrimitive?.contentOrNull}"
        // A judging answer names the decision, not the row it was made on: a
        // pair's number means nothing to the person who answered it.
        Kind.pair_supersede -> "Duplicate pair · keep one"
        Kind.pair_synthesize -> "Duplicate pair · write one"
        Kind.pair_discard -> "Duplicate pair · discard both"
        Kind.pair_dismiss -> "Duplicate pair · dismissed"
        Kind.gap_dismiss -> "Gap · dismissed"
        Kind.gap_forget -> "Gaps · forgotten"
        Kind.artifact_op -> when (p["op"]?.jsonPrimitive?.contentOrNull) {
            "verify" -> "Artifact · still accurate"
            "deprecate" -> "Artifact · hidden"
            "reactivate", "unsupersede" -> "Artifact · back in results"
            else -> "Artifact"
        }
        Kind.artifact_delete -> "Artifact · deleted"
        // The row carries its own wording.
        Kind.call -> p["label"]?.jsonPrimitive?.contentOrNull ?: "Write"
        Kind.merge_undo -> "Merge · undone"
        Kind.corpus_resolve -> when (p["action"]?.jsonPrimitive?.contentOrNull) {
            "replace" -> "Parked capture · replaced the old one"
            "keep_both" -> "Parked capture · kept both"
            "discard" -> "Parked capture · discarded"
            else -> "Parked capture"
        }
    }
}
