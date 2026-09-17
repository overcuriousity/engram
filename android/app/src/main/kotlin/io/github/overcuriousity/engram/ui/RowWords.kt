package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.db.OutboxRow
import io.github.overcuriousity.engram.core.db.State

/** A row's state in words: what the Queue prints under each line. */
fun rowWords(row: OutboxRow, now: Long): String = when (row.state) {
    State.queued -> {
        val wait = row.nextAt - now
        if (wait <= 0) "waiting" else "waiting · next try in ${span(wait)}"
    }
    State.sent -> when (row.status) {
        200 -> "already held"
        202 -> "stored · still being read"
        else -> "stored"
    }
    State.held -> "held for review · ${row.error ?: ""}".trimEnd(' ', '·')
    State.refused -> "refused · scan a new code"
}

private fun span(ms: Long): String {
    val s = ms / 1000
    return when {
        s < 90 -> "${s}s"
        s < 5400 -> "${(s + 30) / 60} min"
        else -> "${(s + 1800) / 3600} h"
    }
}
