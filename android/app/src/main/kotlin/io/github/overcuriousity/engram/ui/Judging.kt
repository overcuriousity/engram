package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.read.Pair
import io.github.overcuriousity.engram.core.read.PairCluster
import io.github.overcuriousity.engram.core.read.SetAsideAction

/**
 * What a duplicate-pair card may say, and which answers it offers.
 *
 * Pure, and separate from the screen for the reason `railOf` is: every rule
 * here is about a card not claiming more than somebody established, and a rule
 * like that has to be checkable without a device.
 *
 * Nothing here decides anything about the pair. The three facts it reads —
 * `unjudged`, `viaLink`, `mergeable` — are the server's, and the whole of this
 * file is what a client is allowed to draw from them.
 */
enum class PairAnswer { KeepA, KeepB, WriteOne, DiscardBoth, Dismiss }

/**
 * All five answers the web offers, not a set reduced for a phone — the
 * decision being made is the same one. *Write one* is left out only where the
 * merge path would refuse a synthesis, because a button whose press can only
 * come back a validation error is worse than no button.
 */
fun answersFor(p: Pair): List<PairAnswer> = buildList {
    add(PairAnswer.KeepA)
    add(PairAnswer.KeepB)
    if (p.mergeable) add(PairAnswer.WriteOne)
    add(PairAnswer.DiscardBoth)
    add(PairAnswer.Dismiss)
}

/**
 * The quiet line over the card: what was measured, and what was found. A pair
 * that came from repeated co-retrieval had no cosine computed at all, so its
 * `percent` is not a similarity and is not shown as one.
 */
fun pairWords(p: Pair): List<String> = buildList {
    add(if (p.viaLink) "recalled together" else "${p.percent}% alike")
    if (p.unjudged) add("unjudged")
    if (p.contradiction) add("contradiction")
    if (p.vacuous) add("says little")
    if (p.synthesisAsked) add("one already asked for")
}

/**
 * The judge's line, where a judge wrote one. Null on an unjudged pair even if
 * a line somehow arrives with it: *these two cover the same ground* is a
 * finding, and on a pair nothing has read it is a claim made by nobody.
 */
fun findingOf(p: Pair): String? = p.finding?.takeIf { !p.unjudged && it.isNotBlank() }

/** As much of a label as a button can carry. */
private fun short(label: String, max: Int = 28): String =
    if (label.length <= max) label else label.take(max).trimEnd() + "…"

/** What the button says. A side is named by what it is, never by its position. */
fun answerWords(a: PairAnswer, p: Pair): String = when (a) {
    PairAnswer.KeepA -> "Keep \"${short(p.a.label)}\""
    PairAnswer.KeepB -> "Keep \"${short(p.b.label)}\""
    PairAnswer.WriteOne -> "Write one"
    PairAnswer.DiscardBoth -> "Discard both"
    PairAnswer.Dismiss -> "Dismiss"
}

/**
 * What the answer costs, named before it is made, as the web's confirmation
 * names it. Null where nothing is hidden and there is nothing to confirm.
 */
fun answerCost(a: PairAnswer, p: Pair): String? = when (a) {
    PairAnswer.KeepA -> "Hides \"${short(p.b.label)}\"."
    PairAnswer.KeepB -> "Hides \"${short(p.a.label)}\"."
    PairAnswer.WriteOne -> "Writes one from both, and retires them."
    PairAnswer.DiscardBoth -> "Retires both."
    PairAnswer.Dismiss -> null
}

/**
 * One card. A cluster is one question — one artifact against three others is
 * not three questions — so a card carries how many pairs its cluster holds,
 * and says so where that is more than one. The card is still one pair at a
 * time: what is being decided is which of *these two* stays.
 */
data class PairCard(val pair: Pair, val siblings: Int)

fun cardsOf(clusters: List<PairCluster>): List<PairCard> =
    clusters.flatMap { c -> c.pairs.map { PairCard(it, c.pairs.size) } }

/** What a set-aside row's button says. The answer is the server's; the words are ours. */
fun setAsideWords(a: SetAsideAction): String = when (a) {
    SetAsideAction.Verify -> "Still accurate"
    SetAsideAction.Deprecate -> "Hide"
    SetAsideAction.Reactivate -> "Return to results"
    SetAsideAction.UndoMerge -> "Undo the merge"
    SetAsideAction.ResolveReplace -> "Replace the old one"
    SetAsideAction.ResolveKeepBoth -> "Keep both"
    SetAsideAction.ResolveDiscard -> "Discard this"
}

/** How a gap cluster came by its name. A name a model gave is a reading; one taken from the words is not. */
fun labelledWords(labelledBy: String): String = when (labelledBy) {
    "model" -> "named by a model"
    "terms" -> "from the words"
    else -> labelledBy
}
