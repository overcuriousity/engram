package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.read.Pair
import io.github.overcuriousity.engram.core.read.PairSide
import org.junit.Assert.*
import org.junit.Test

/**
 * What a pair card is allowed to say, and which answers it offers. Pure, the
 * way `railOf` is: every one of these rules is a claim about honesty, and a
 * claim about honesty that only exists inside a `@Composable` is one nobody
 * can check.
 */
class JudgeTest {
    private fun pair(
        percent: Long = 91,
        viaLink: Boolean = false,
        finding: String? = "both give the timeout, and they disagree",
        unjudged: Boolean = false,
        mergeable: Boolean = true,
        contradiction: Boolean = false,
        vacuous: Boolean = false,
        keeps: String? = null,
    ) = Pair(
        id = 1,
        percent = percent,
        viaLink = viaLink,
        a = PairSide("art-a", "Timeout 0", named = true, excerpt = "the timeout is 30 seconds"),
        b = PairSide("art-b", "Timeout 1", named = true, excerpt = "the timeout is 90 seconds"),
        finding = finding,
        contradiction = contradiction,
        vacuous = vacuous,
        unjudged = unjudged,
        mergeable = mergeable,
        keeps = keeps,
    )

    @Test fun allFiveAnswersAreOfferedAndTheWebIsNotReducedForThePhone() {
        assertEquals(
            listOf(
                PairAnswer.KeepA,
                PairAnswer.KeepB,
                PairAnswer.WriteOne,
                PairAnswer.DiscardBoth,
                PairAnswer.Dismiss,
            ),
            answersFor(pair()),
        )
    }

    @Test fun whereAMergeWouldBeRefusedThereIsNoButtonToPress() {
        val answers = answersFor(pair(mergeable = false))
        assertFalse(PairAnswer.WriteOne in answers)
        assertEquals("and the other four stay", 4, answers.size)
    }

    @Test fun anUnjudgedPairDrawsTheMeasurementAndNoFinding() {
        // The sweep filed this on a cosine score and nothing has read it since.
        // "These two cover the same ground" is a finding nobody made.
        val p = pair(unjudged = true)
        assertNull(findingOf(p))
        assertTrue("91% alike" in pairWords(p))
        assertTrue("unjudged" in pairWords(p))
    }

    @Test fun aJudgedPairDrawsWhatTheJudgeWrote() {
        val p = pair()
        assertEquals("both give the timeout, and they disagree", findingOf(p))
        assertFalse("unjudged" in pairWords(p))
    }

    @Test fun aPairFromCoRetrievalShowsNoPercentage() {
        // No cosine was ever computed, so `percent` is not a similarity.
        val words = pairWords(pair(viaLink = true, percent = 91))
        assertFalse(words.any { it.contains("%") })
        assertTrue("recalled together" in words)
    }

    @Test fun whatTheJudgeFoundIsSaidInItsOwnWords() {
        val words = pairWords(pair(contradiction = true, vacuous = true))
        assertTrue("contradiction" in words)
        assertTrue("says little" in words)
    }

    @Test fun keepingASideNamesTheSideAndNotAPosition() {
        // "Keep A" tells a person nothing about what they are keeping.
        assertEquals("""Keep "Timeout 0"""", answerWords(PairAnswer.KeepA, pair()))
        assertEquals("""Keep "Timeout 1"""", answerWords(PairAnswer.KeepB, pair()))
        assertEquals("Write one", answerWords(PairAnswer.WriteOne, pair()))
    }

    @Test fun anAnswerNamesWhatItHides() {
        // What the confirmation says, as the web's does.
        assertEquals("""Hides "Timeout 1".""", answerCost(PairAnswer.KeepA, pair()))
        assertEquals("""Hides "Timeout 0".""", answerCost(PairAnswer.KeepB, pair()))
        assertEquals("Retires both.", answerCost(PairAnswer.DiscardBoth, pair()))
        assertEquals("Writes one from both, and retires them.", answerCost(PairAnswer.WriteOne, pair()))
        // Nothing is hidden, so there is nothing to confirm.
        assertNull(answerCost(PairAnswer.Dismiss, pair()))
    }

    @Test fun aSideWithNoNameIsQuotedByItsOpening() {
        // It stands in for a name and is not one; it still has to be what the
        // button names, because the alternative is naming a position.
        val p = pair().copy(a = PairSide("art-a", "the timeout is 30 seconds", named = false, excerpt = "x"))
        assertEquals("""Keep "the timeout is 30 seconds"""", answerWords(PairAnswer.KeepA, p))
    }

    @Test fun aLabelTooLongForAButtonIsCutAndSaysSo() {
        val long = "the timeout is thirty seconds unless the caller asks for longer, in which case"
        val p = pair().copy(a = PairSide("art-a", long, named = false, excerpt = "x"))
        val words = answerWords(PairAnswer.KeepA, p)
        assertTrue(words.length < long.length)
        assertTrue(words.endsWith("…\""))
        assertTrue(words.startsWith("""Keep "the timeout is"""))
    }
}
