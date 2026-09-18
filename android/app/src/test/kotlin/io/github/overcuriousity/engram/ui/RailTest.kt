package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.read.Hit
import org.junit.Assert.*
import org.junit.Test

/**
 * Where the rule goes and what a row may say, *given* the server's flags.
 * Nothing here says which hit deserved a flag or what order hits belong in:
 * ranking is the server's, and a second opinion about it kept in Kotlin is a
 * second definition of correct.
 */
class RailTest {
    private fun hit(id: String, weak: Boolean = false, past: Boolean = false, via: String? = null) =
        Hit(artifactId = id, weak = weak, pastCliff = past, via = via)

    @Test fun theRuleGoesOnceAboveTheFirstRowPastIt() {
        val rail = railOf(listOf(hit("a"), hit("b"), hit("c", past = true), hit("d", past = true)))
        assertEquals(1, rail.count { it is RailItem.Cliff })
        assertEquals(2, rail.indexOf(RailItem.Cliff))
        assertEquals("c", (rail[3] as RailItem.Row).hit.artifactId)
    }

    @Test fun aListThatNeverFallsOffHasNoRule() {
        assertTrue(railOf(listOf(hit("a"), hit("b"))).none { it is RailItem.Cliff })
    }

    @Test fun aListThatIsAllPastTheRuleOpensWithIt() {
        assertEquals(RailItem.Cliff, railOf(listOf(hit("a", past = true))).first())
    }

    @Test fun rowsPastTheRuleKeepTheirRankAndAreDrawnAsPast() {
        val rows = railOf(listOf(hit("a"), hit("b", past = true), hit("c", past = true))).filterIsInstance<RailItem.Row>()
        assertEquals(listOf(1, 2, 3), rows.map { it.rank })
        assertEquals(listOf(false, true, true), rows.map { it.past })
    }

    @Test fun aLooseHitAndARecalledOneHaveNoRank() {
        val rows = railOf(listOf(hit("a"), hit("b", weak = true), hit("c", via = "a"))).filterIsInstance<RailItem.Row>()
        assertEquals(listOf(1, null, null), rows.map { it.rank })
        assertEquals(listOf(false, true, false), rows.map { it.loose })
    }

    @Test fun whenEveryHitIsLooseItIsSaidOnceAndNotOnEachRow() {
        val rail = railOf(listOf(hit("a", weak = true), hit("b", weak = true)))
        assertEquals(RailItem.NothingClose, rail.first())
        assertTrue(rail.filterIsInstance<RailItem.Row>().none { it.loose || it.rank != null })
    }

    @Test fun anEmptyListIsEmpty() = assertTrue(railOf(emptyList()).isEmpty())

    @Test fun aBorrowedNameIsNotShownAsTheTextsOwn() {
        assertEquals("Kapitel 3" to true, nameOf(Hit("a", title = "Kapitel 3", text = "Der Vorgang")))
        assertEquals("Der Vorgang" to false, nameOf(Hit("a", title = "Kapitel 3", text = "Der Vorgang", borrowedName = true)))
        assertEquals("Der Vorgang" to false, nameOf(Hit("a", title = null, text = "Der  Vorgang\n")))
    }

    @Test fun anOpeningIsCutAtAWord() {
        assertEquals("one two…", opening("one two three", 9))
        assertEquals("short", opening("short", 60))
    }

    /**
     * A label costs what a label costs. `opening` reads markdown, and what it
     * is handed can be a whole capture — a megabyte of PDF for sixty
     * characters — so it reads only as far as it needs and answers the same
     * either way.
     */
    @Test fun aLabelIsTheSameWhateverFollowsIt() {
        val head = "**Alpha** bravo charlie delta echo foxtrot golf hotel india juliet kilo lima"
        assertEquals(opening(head, 60), opening(head + "\n\ntail ".repeat(20_000), 60))
    }

    /** A text that is almost all markup still gives up its words: the rest is read. */
    @Test fun aTextThatFlattensToAlmostNothingIsReadToTheEnd() {
        // Twenty links, each four hundred characters of address and one of
        // words: what is read first is full long before the words are.
        val filler = "[a](https://example.invalid/${"x".repeat(400)})\n\n".repeat(20)
        assertTrue(opening(filler + "and then the words", 60).endsWith("and then the words"))
    }

    @Test fun theSmallWordsSayWhyARowIsWhereItIs() {
        assertEquals(
            listOf("done reminder", "primed · seen", "due in 2 h", "model-written · 3"),
            wordsOf(Hit("a", retired = true, primed = true, inSitting = true, dueIn = "in 2 h", modelWritten = true, originCount = 3)),
        )
        assertTrue(wordsOf(Hit("a")).isEmpty())
    }
}
