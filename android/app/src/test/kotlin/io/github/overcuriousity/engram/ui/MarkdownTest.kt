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
        for (s in listOf("", "*", "**", "[", "[a](", "```", "|", "|-|", "> ", "- ", "1. ", "\\", "`", "$", "$$", "$$$", "${'$'}a", "a$", "\\$", "{$", "$}$")) parseMarkdown(s)
    }

    // ── Math, read as pulldown-cmark reads it (the web's `markdown.rs`) ──

    private fun inl(s: String) = (parseMarkdown(s).single() as Block.Paragraph).inlines
    private fun inlineMath(tex: String) = Inline.Math(tex, display = false)

    /** The answer from #129 that showed its source on the phone. */
    @Test fun thePerceptronUpdateIsADisplayedFormulaAndEtaAnInlineOne() {
        val rule = "w_i \\leftarrow w_i + \\eta \\cdot c \\cdot [h(x) - c(x)]"
        val doc = parseMarkdown(
            "Die Lernregel:\n\n\$\$$rule\$\$\n\n- **Lernrate \$\\eta\$:** klein\n- mit \$\\eta > 0\$ und *Ziel* \$c(x)\$",
        )
        assertEquals(Block.Paragraph(text("Die Lernregel:")), doc[0])
        assertEquals(Block.Paragraph(listOf(Inline.Math(rule, display = true))), doc[1])
        val list = doc[2] as Block.ListBlock
        assertEquals(
            Block.Paragraph(listOf(Inline.Strong(listOf(Inline.Text("Lernrate "), inlineMath("\\eta"), Inline.Text(":"))), Inline.Text(" klein"))),
            list.items[0].single(),
        )
        assertEquals(
            Block.Paragraph(listOf(Inline.Text("mit "), inlineMath("\\eta > 0"), Inline.Text(" und "), Inline.Emph(text("Ziel")), Inline.Text(" "), inlineMath("c(x)"))),
            list.items[1].single(),
        )
    }

    /** `$$` on lines of its own, as a model writes it, inside a paragraph that goes on. */
    @Test fun aDisplayedFormulaMaySpanLinesInsideAParagraph() {
        assertEquals(
            listOf(Inline.Text("Es gilt "), Inline.Math("\na^2 + b^2 = c^2\n", display = true), Inline.Text(" für jedes Dreieck.")),
            inl("Es gilt\n$$\na^2 + b^2 = c^2\n$$\nfür jedes Dreieck."),
        )
    }

    @Test fun pricesAreNotMath() {
        assertEquals(text("Es kostet 5$ und 10$."), inl("Es kostet 5$ und 10$."))
        // Opens at `$5`, but the next `$` follows a space and cannot close: no formula at all.
        assertEquals(text("from $5 to $10"), inl("from $5 to $10"))
        assertEquals(text("a $ b $ c"), inl("a $ b $ c"))
        assertEquals(text("costs $5"), inl("costs $5"))
    }

    @Test fun anEscapedDollarIsADollarAndInsideAFormulaStaysTeX() {
        assertEquals(text("\$x\$ is literal"), inl("\\\$x\\\$ is literal"))
        assertEquals(listOf(inlineMath("a \\$ b"), Inline.Text(" ok")), inl("\$a \\\$ b\$ ok"))
    }

    @Test fun aDollarInCodeIsCodeAndCodeInAFormulaIsTeX() {
        assertEquals(listOf(Inline.Code("\$x\$"), Inline.Text(" and "), inlineMath("y")), inl("`\$x\$` and \$y\$"))
        assertEquals(listOf(inlineMath("a `b` c")), inl("\$a `b` c\$"))
    }

    @Test fun aDollarInsideBracesBelongsToThemAndUnbalancedBracesAreNoFormula() {
        assertEquals(listOf(inlineMath("\\text{\$x\$} + 1")), inl("\$\\text{\$x\$} + 1\$"))
        assertEquals(text("\$}\$"), inl("\$}\$"))
    }

    /** A `$$` that does not close is two dollars; the second may open a formula of its own. */
    @Test fun anUnclosedDisplayIsADollarAndAnInlineFormula() {
        assertEquals(listOf(Inline.Text("$"), inlineMath("x")), inl("\$\$x\$"))
    }

    @Test fun headingsAndTableCellsHoldFormulasToo() {
        assertEquals(Block.Heading(2, listOf(Inline.Text("Satz über "), inlineMath("\\pi"))), parseMarkdown("## Satz über \$\\pi\$").single())
        val t = parseMarkdown("| a | b |\n|---|---|\n| \$x^2\$ | 2 |").single() as Block.Table
        assertEquals(listOf(inlineMath("x^2")), t.rows[0][0])
    }

    /** The server's snippet keeps a formula's source, dollars dropped. */
    @Test fun plainKeepsTheSource() {
        assertEquals("Mit \\eta > 0 gilt w_i \\leftarrow w_i", plain("Mit \$\\eta > 0\$ gilt\n\n\$\$w_i \\leftarrow w_i\$\$"))
    }

    /** A soft break inside emphasis no longer ends it: the paragraph is read whole, as CommonMark reads it. */
    @Test fun emphasisMayCrossALineEnd() {
        assertEquals(listOf(Inline.Text("a "), Inline.Strong(text("b c")), Inline.Text(" d")), inl("a **b\nc** d"))
    }
}
