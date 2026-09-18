package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.read.Hit

/**
 * A result list as it is drawn: rows, and at most one rule between them.
 *
 * The rule that says *relevance falls off here*, and the rows beneath it that
 * keep their rank and stop claiming to be answers, are the reason to prefer
 * this search over a box that always looks confident. They are not chrome.
 * Retrieval always returns its best candidates however bad they are, so a list
 * without them shows a typo's results exactly as it shows an answer's.
 *
 * Nothing here orders anything. The order is the server's, and so are the two
 * facts this reads — `pastCliff` and `weak`. This only decides where a line
 * goes and what a row is allowed to say about itself.
 */
sealed interface RailItem {
    data class Row(
        val hit: Hit,
        /** Its place in the list, or null where a rank would be a false claim. */
        val rank: Int?,
        /** Badged *loose* in place of a rank. */
        val loose: Boolean,
        /** Beneath the rule: drawn muted. */
        val past: Boolean,
    ) : RailItem

    /** *Relevance falls off here.* Once, above the first row past it. */
    data object Cliff : RailItem

    /** Every hit is loose. Said once, over the list, instead of on every row. */
    data object NothingClose : RailItem
}

fun railOf(hits: List<Hit>): List<RailItem> {
    // When the whole list is loose the notice above it has said so, and a
    // badge on every row says it again, and again.
    val allLoose = hits.isNotEmpty() && hits.all { it.weak }
    val out = mutableListOf<RailItem>()
    if (allLoose) out += RailItem.NothingClose
    hits.forEachIndexed { i, h ->
        if (h.pastCliff && (i == 0 || !hits[i - 1].pastCliff)) out += RailItem.Cliff
        out += RailItem.Row(
            hit = h,
            // A rank is a claim about standing among answers. A loose hit is
            // not one, and a hit recalled beside another never competed. Past
            // the rule the ranks continue — those hits did place.
            rank = if (h.weak || h.via != null) null else i + 1,
            loose = h.weak && !allLoose,
            past = h.pastCliff,
        )
    }
    return out
}

/**
 * What a row is called. A borrowed name — a section heading, or the note's
 * name lent to an untitled passage — is not shown as this text's name; the row
 * is then named by how it opens, and says it is not a name.
 */
fun nameOf(h: Hit): Pair<String, Boolean> {
    val title = h.title?.takeIf { it.isNotBlank() && !h.borrowedName }
    return if (title != null) title to true else opening(h.text, 60) to false
}

/**
 * The first `max` characters of a text on one line, cut at a word where there
 * is one. The words, not the markup: a row that opened with `## Öffnungszeiten
 * **Adresse:**` was showing syntax where the web shows a sentence — its
 * snippet goes through `markdown::snippet`, and this is that reading.
 */
fun opening(text: String, max: Int): String {
    val head = headOf(text, maxOf(WINDOW, max * 4))
    var flat = plain(head)
    // Almost all of what was read was markup — a page of addresses, or a
    // table of rules — so the words are further in than the window reached.
    if (flat.length <= max && head.length < text.length) flat = plain(text)
    if (flat.length <= max) return flat
    val cut = flat.substring(0, max)
    val at = cut.lastIndexOf(' ')
    return (if (at > max / 2) cut.substring(0, at) else cut).trimEnd() + "…"
}

/**
 * As much of a text as [opening] reads before deciding it has enough.
 *
 * `plain` parses everything it is handed, and some of what is handed here is
 * a whole document — a corpus's text is the entire PDF, read to label a row
 * with sixty characters. Markup never grows a text, so a few thousand
 * characters hold the answer. The cut is made at a paragraph break, or a line
 * break where there is no paragraph: markup severed halfway comes back as the
 * asterisks this function exists to take away.
 */
private fun headOf(text: String, window: Int): String {
    if (text.length <= window) return text
    val at = text.lastIndexOf("\n\n", window).takeIf { it > 0 }
        ?: text.lastIndexOf('\n', window).takeIf { it > 0 }
    return text.take(at ?: window)
}

/** The least of a text [opening] reads before it decides it has enough. */
private const val WINDOW = 4096

/**
 * Where a row's document goes on, said on the row: the rank of the next
 * passage where it placed in this list, or that it did not — exclusive by
 * construction, as the web's `mark_continuations` makes them.
 */
fun continuesWords(h: Hit, items: List<RailItem>): String? {
    val next = h.continuesTo ?: return null
    val rank = items.filterIsInstance<RailItem.Row>().firstOrNull { it.hit.artifactId == next }?.rank
    return if (rank != null) "↓ continues in #$rank" else "↳ continues in the next passage"
}

/**
 * Where in its source a passage sits, under the snippet and prefixed. A
 * borrowed name is the heading of the section it was cut from, and emptying
 * the name slot for it would take the row's only statement of whereabouts.
 */
fun sectionOf(h: Hit): String? =
    h.title?.takeIf { h.borrowedName && it.isNotBlank() && !plain(h.text).startsWith(it) }

/**
 * Why this row is *here*, as one sentence rather than a row of chips — the
 * web's `rail-why`. The badges say what a result is; this says why.
 */
fun whyOf(h: Hit, allLoose: Boolean): String? = buildList {
    if (h.primed) add(if (h.inSitting) "moved up — you have been in this one already" else "moved up — opened, confirmed or cited more than the hits it passed")
    if (h.weak && !allLoose) add("a loose match")
    if (h.modelWritten) add("written by a model" + if (h.originCount > 0) " from ${h.originCount} source${if (h.originCount > 1) "s" else ""}" else "")
    h.dueIn?.let { add("a reminder on this is due $it") }
    h.whyRanked?.let { add(it) }
}.takeIf { it.isNotEmpty() }?.joinToString(" · ")

/** The small words under a row, in the order the web rail says them. */
fun wordsOf(h: Hit): List<String> = buildList {
    if (h.retired) add("done reminder")
    if (h.primed) add(if (h.inSitting) "primed · seen" else "primed")
    h.dueIn?.let { add("due $it") }
    if (h.modelWritten) add(if (h.originCount > 0) "model-written · ${h.originCount}" else "model-written")
    if (h.via != null) add(h.reason ?: "associated")
}
