package io.github.overcuriousity.engram.ui

import kotlinx.coroutines.FlowPreview
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.debounce
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map

/**
 * How long the typing has to stand still before it is a question. The web's
 * number, from `workspace.html`: a quarter second is a pause you can feel
 * between a phrase and its answer, and the work is the same either way — one
 * embedding, one vector search, never a generation.
 */
internal const val SETTLES_MS = 120L

/**
 * The questions a box asks, given every state it passes through. Three things
 * happen here and nothing else: the text is trimmed, so the space after a word
 * is not a question of its own; it settles, so the letters on the way to a
 * word are not asked for; and it is deduplicated, so an edit that leaves the
 * trimmed text alone asks nothing.
 *
 * What is *not* here is the cancelling. A question that arrives while the one
 * before it is still in flight has to win, and on the web that is
 * `hx-sync="this:replace"`. Here it is `rememberRead`, keyed on the request:
 * the effect holding the read is cancelled and restarted when the key changes,
 * so the answer on screen is always the answer to the last thing typed. The
 * debounce is a kindness to the server; the cancelling is the correctness.
 */
@OptIn(FlowPreview::class)
internal fun Flow<String>.queries(): Flow<String> =
    map { it.trim() }.debounce(SETTLES_MS).distinctUntilChanged()
