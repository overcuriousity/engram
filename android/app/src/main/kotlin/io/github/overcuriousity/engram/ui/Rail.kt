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
    val flat = plain(text)
    if (flat.length <= max) return flat
    val cut = flat.substring(0, max)
    val at = cut.lastIndexOf(' ')
    return (if (at > max / 2) cut.substring(0, at) else cut).trimEnd() + "…"
}

/** The small words under a row, in the order the web rail says them. */
fun wordsOf(h: Hit): List<String> = buildList {
    if (h.retired) add("done reminder")
    if (h.primed) add(if (h.inSitting) "primed · seen" else "primed")
    h.dueIn?.let { add("due $it") }
    if (h.modelWritten) add(if (h.originCount > 0) "model-written · ${h.originCount}" else "model-written")
    if (h.via != null) add(h.reason ?: "associated")
}
