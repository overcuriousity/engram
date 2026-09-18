package io.github.overcuriousity.engram.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The dialect the server renders — pulldown-cmark with tables and
 * strikethrough — read into the tree the screen draws. Plain JUnit: nothing
 * here needs Android, which is the point of keeping the reading apart from
 * the drawing.
 */
class MarkdownTest {
    private fun text(vararg t: String) = t.map { Inline.Text(it) }

    @Test fun aCapturedNoteReadsAsHeadingsBoldAndAList() {
        val doc = parseMarkdown(
            """
            ## Öffnungszeiten Wertstoffhof

            **Adresse:** Thürhamer Straße 21a

            **Öffnungszeiten:**
            - Montag: geschlossen
            - Dienstag: 08:00–12:30 Uhr
            """.trimIndent(),
        )
        assertEquals(
            listOf(
                Block.Heading(2, text("Öffnungszeiten Wertstoffhof")),
                Block.Paragraph(listOf(Inline.Strong(text("Adresse:")), Inline.Text(" Thürhamer Straße 21a"))),
                Block.Paragraph(listOf(Inline.Strong(text("Öffnungszeiten:")))),
                Block.ListBlock(false, 1, listOf(listOf(Block.Paragraph(text("Montag: geschlossen"))), listOf(Block.Paragraph(text("Dienstag: 08:00–12:30 Uhr"))))),
            ),
            doc,
        )
    }

    /** The row under a result shows words, never the syntax. */
    @Test fun plainIsTheWordsOnOneLine() {
        assertEquals(
            "Öffnungszeiten Wertstoffhof Adresse: Thürhamer Straße 21a Montag: geschlossen",
            plain("## Öffnungszeiten Wertstoffhof\n\n**Adresse:** Thürhamer Straße 21a\n- Montag: geschlossen"),
        )
        assertEquals("run ls -la now", plain("run `ls -la` now"))
        assertEquals("a link", plain("[a link](https://x.example)"))
        assertEquals("nothing", plain("nothing"))
    }

    @Test fun codeIsKeptAsWrittenAndInlineCodeMayHoldABacktick() {
        val doc = parseMarkdown("```sh\n# not a heading\n  indented\n```\n\nuse `` a`b `` here")
        assertEquals(Block.Code("# not a heading\n  indented", "sh"), doc[0])
        assertEquals(Block.Paragraph(listOf(Inline.Text("use "), Inline.Code("a`b"), Inline.Text(" here"))), doc[1])
    }

    @Test fun listsNestByIndentAndAnOrderedListKeepsItsStart() {
        val doc = parseMarkdown("3. three\n4. four\n   - inner\n   - inner two\n5. five")
        val list = doc.single() as Block.ListBlock
        assertTrue(list.ordered)
        assertEquals(3, list.start)
        assertEquals(3, list.items.size)
        val second = list.items[1]
        assertEquals(Block.Paragraph(text("four")), second[0])
        val inner = second[1] as Block.ListBlock
        assertEquals(listOf("inner", "inner two"), inner.items.map { (it.single() as Block.Paragraph).inlines.single().let { i -> (i as Inline.Text).text } })
    }

    @Test fun aLazyLineContinuesTheItemAndABlankLineDoesNotEndTheList() {
        val doc = parseMarkdown("- one\ncontinued\n\n- two")
        val list = doc.single() as Block.ListBlock
        assertEquals(2, list.items.size)
        assertEquals(Block.Paragraph(text("one continued")), list.items[0].single())
    }

    @Test fun aTableIsItsHeaderAndRows() {
        val doc = parseMarkdown("| a | b |\n|---|:-:|\n| 1 | `x|y` |\n| 2 |")
        val t = doc.single() as Block.Table
        assertEquals(listOf(text("a"), text("b")), t.header)
        assertEquals(2, t.rows.size)
        assertEquals(listOf(Inline.Code("x|y")), t.rows[0][1])
        assertEquals(emptyList<Inline>(), t.rows[1][1])
    }

    @Test fun quotesRulesAndSetextHeadings() {
        val doc = parseMarkdown("> quoted\n> still\n\n---\n\nTitle\n=====\n\nSub\n---")
        assertEquals(Block.Quote(listOf(Block.Paragraph(text("quoted still")))), doc[0])
        assertEquals(Block.Rule, doc[1])
        assertEquals(Block.Heading(1, text("Title")), doc[2])
        assertEquals(Block.Heading(2, text("Sub")), doc[3])
    }

    @Test fun linksBareUrlsAndEscapes() {
        val p = parseMarkdown("see [the docs](https://x.example/a \"t\") or https://y.example/b. Not \\*bold\\*.").single() as Block.Paragraph
        assertEquals(
            listOf(
                Inline.Text("see "),
                Inline.Link(text("the docs"), "https://x.example/a"),
                Inline.Text(" or "),
                Inline.Link(text("https://y.example/b"), "https://y.example/b"),
                Inline.Text(". Not *bold*."),
            ),
            p.inlines,
        )
    }

    @Test fun emphasisThatNeverClosesIsText() {
        assertEquals(Block.Paragraph(text("2 * 3 * 4 and snake_case_name and *dangling")), parseMarkdown("2 * 3 * 4 and snake_case_name and *dangling").single())
        assertEquals(Block.Paragraph(listOf(Inline.Emph(text("em")), Inline.Text(" "), Inline.Strike(text("gone")))), parseMarkdown("*em* ~~gone~~").single())
    }

    @Test fun twoTrailingSpacesAreAHardBreakAndOneNewlineIsASpace() {
        val p = parseMarkdown("one  \ntwo\nthree").single() as Block.Paragraph
        assertEquals(listOf(Inline.Text("one"), Inline.Break, Inline.Text("two three")), p.inlines)
    }

    @Test fun nothingThrows() {
        for (s in listOf("", "*", "**", "[", "[a](", "```", "|", "|-|", "> ", "- ", "1. ", "\\", "`")) parseMarkdown(s)
    }
}
