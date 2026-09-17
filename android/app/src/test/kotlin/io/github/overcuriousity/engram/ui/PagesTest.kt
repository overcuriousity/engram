package io.github.overcuriousity.engram.ui

import io.github.overcuriousity.engram.core.read.CorpusRow
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * The library's pages, flattened. Each page is a read of its own with a cursor
 * that is fixed once asked for, so two pages can overlap while nobody is
 * holding either of them wrong.
 */
class PagesTest {
    private fun rows(vararg ids: String) = ids.map { CorpusRow(it) }

    /**
     * Page two keeps asking from the row that *was* page one's last. Delete a
     * corpus on the server and page one's next revalidation reaches one row
     * further down, so its last row is page two's first — and `LazyColumn`,
     * keyed by id, throws on the repeated key rather than drawing it twice.
     */
    @Test fun aRowTwoPagesBothReachIsListedOnce() {
        val pages = listOf(rows("a", "b", "c"), rows("c", "d"))
        assertEquals(rows("a", "b", "c", "d").map { it.id }, rowsOf(pages).map { it.id })
    }

    @Test fun andPagesThatDoNotOverlapAreLeftInTheirOrder() {
        val pages = listOf(rows("a", "b"), rows("c", "d"))
        assertEquals(rows("a", "b", "c", "d").map { it.id }, rowsOf(pages).map { it.id })
        assertEquals(emptyList<String>(), rowsOf(emptyList()).map { it.id })
    }
}
