package io.github.overcuriousity.engram.ui

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import ru.noties.jlatexmath.JLatexMathAndroid

/**
 * Formulas laid out by JLaTeXMath and set into the text. Native graphics, so
 * the fonts it reads from its assets are real fonts and a width is a width.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [35])
class MathTest {
    @get:Rule val compose = createComposeRule()

    private val perceptron = "w_i \\leftarrow w_i + \\eta \\cdot c \\cdot [h(x) - c(x)]"

    @Before fun fonts() = JLatexMathAndroid.init(ApplicationProvider.getApplicationContext())

    @Test fun theFormulasFromTheAnswerAreLaidOut() {
        val shown = typeset(Inline.Math(perceptron, display = true), 48f, 0xFF000000.toInt())
        assertNotNull(shown)
        val eta = typeset(Inline.Math("\\eta", display = false), 48f, 0xFF000000.toInt())!!
        assertTrue("wider than one letter: ${shown!!.width} vs ${eta.width}", shown.width > 5 * eta.width)
        assertTrue("η has a tail below the line", eta.depth > 0)
    }

    /** What the web does: a formula its renderer cannot read is shown as its source. */
    @Test fun aCommandJLaTeXMathDoesNotKnowIsNoFormula() {
        assertNull(typeset(Inline.Math("\\notacommandanywhere{x}", display = false), 48f, 0xFF000000.toInt()))
    }

    @Test fun aDocumentWithFormulasComposesAndABadOneShowsItsSource() {
        compose.setContent {
            EngramTheme {
                Markdown("Mit \$\\eta > 0\$ gilt:\n\n\$\$$perceptron\$\$\n\nund \$\\notacommandanywhere\$.")
            }
        }
        compose.onNodeWithContentDescription(perceptron).assertExists()
        // The bad one falls back to code, which is its source in the text.
        compose.onNodeWithText("\\notacommandanywhere", substring = true).assertExists()
        // The good inline one is in the text too, as the source it stands in for.
        compose.onNodeWithText("\\eta > 0", substring = true).assertExists()
    }

    @Test fun aParagraphIsCutAtItsDisplayedFormulas() {
        val p = parseMarkdown("Es gilt\n\$\$a = b\$\$\nimmer.").single() as Block.Paragraph
        assertEquals(
            listOf(
                Run.Words(listOf(Inline.Text("Es gilt"))),
                Run.Shown(Inline.Math("a = b", display = true)),
                Run.Words(listOf(Inline.Text("immer."))),
            ),
            runs(p.inlines),
        )
    }
}
