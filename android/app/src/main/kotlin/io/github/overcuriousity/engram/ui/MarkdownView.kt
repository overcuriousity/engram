package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.InlineTextContent
import androidx.compose.foundation.text.appendInlineContent
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.takeOrElse
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.Placeholder
import androidx.compose.ui.text.PlaceholderVerticalAlign
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.withLink
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.unit.takeOrElse
import androidx.core.graphics.withSave
import ru.noties.jlatexmath.JLatexMathAndroid
import ru.noties.jlatexmath.JLatexMathDrawable

/**
 * An artifact's text, drawn as the web draws it: the markdown a model wrote
 * as headings, emphasis, lists and the rest — never as its syntax. The
 * reading is `Markdown.kt`; this is only the drawing.
 */
@Composable
fun Markdown(
    text: String,
    modifier: Modifier = Modifier,
    style: TextStyle = MaterialTheme.typography.bodyLarge,
    /** Literals to mark as no excerpt's: coloured and underlined, so the mark survives without colour. */
    mark: List<String> = emptyList(),
    /** Where a `cite:n` link goes — an answer's `[n]` — instead of a browser. */
    onCite: ((Int) -> Unit)? = null,
) {
    val blocks = remember(text) { parseMarkdown(text) }
    val marking = Marking(mark, SpanStyle(color = MaterialTheme.colorScheme.secondary, textDecoration = TextDecoration.Underline), onCite)
    SelectionContainer(modifier) {
        Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
            blocks.forEach { BlockView(it, style, marking) }
        }
    }
}

/** What an answer's rendering carries that an artifact's does not. */
class Marking(val literals: List<String>, val style: SpanStyle, val onCite: ((Int) -> Unit)?)

private val NO_MARKING = Marking(emptyList(), SpanStyle(), null)

/**
 * A passage, as the document wrote it. Markdown is the wrong reader for a
 * slice of a document: it eats the `#` of a section number and joins the
 * lines whose breaks are the structure. The web puts it in a `<pre>`; this is
 * that, wrapping at the edge rather than scrolling off it.
 */
@Composable
fun Verbatim(text: String, modifier: Modifier = Modifier) {
    SelectionContainer(modifier) {
        Text(text, style = MaterialTheme.typography.bodyMedium.copy(fontFamily = Mono, lineHeight = 20.sp))
    }
}

@Composable
private fun BlockView(b: Block, style: TextStyle, m: Marking = NO_MARKING) {
    when (b) {
        is Block.Heading -> InlineText(
            b.inlines,
            m,
            style = when (b.level) {
                1 -> MaterialTheme.typography.titleLarge
                2 -> MaterialTheme.typography.titleMedium
                else -> MaterialTheme.typography.labelLarge
            },
            modifier = Modifier.padding(top = if (b.level <= 2) 6.dp else 2.dp),
        )
        is Block.Paragraph -> Paragraph(b.inlines, style, m)
        is Block.Code -> Text(
            b.text,
            Modifier.fillMaxWidth()
                .background(MaterialTheme.colorScheme.surfaceContainer, MaterialTheme.shapes.small)
                .horizontalScroll(rememberScrollState())
                .padding(10.dp, 8.dp),
            style = MaterialTheme.typography.bodySmall.copy(fontFamily = Mono),
            softWrap = false,
        )
        is Block.Quote -> Row(Modifier.height(IntrinsicSize.Min)) {
            Box(Modifier.width(3.dp).fillMaxHeight().background(MaterialTheme.colorScheme.outline))
            Column(Modifier.padding(start = 12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                b.blocks.forEach { BlockView(it, style.copy(color = MaterialTheme.colorScheme.onSurfaceVariant), m) }
            }
        }
        is Block.ListBlock -> Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
            b.items.forEachIndexed { i, item ->
                Row {
                    Text(
                        if (b.ordered) "${b.start + i}." else "•",
                        Modifier.width(24.dp),
                        style = style,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        item.forEach { BlockView(it, style, m) }
                    }
                }
            }
        }
        is Block.Table -> Column(Modifier.fillMaxWidth()) {
            Row(Modifier.fillMaxWidth()) {
                b.header.forEach { cell ->
                    InlineText(cell, m, style.copy(fontWeight = FontWeight.Medium), Modifier.weight(1f).padding(4.dp, 6.dp))
                }
            }
            HorizontalDivider(color = MaterialTheme.colorScheme.outline)
            b.rows.forEach { row ->
                Row(Modifier.fillMaxWidth()) {
                    row.forEach { cell -> InlineText(cell, m, style, Modifier.weight(1f).padding(4.dp, 6.dp)) }
                }
                HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
            }
        }
        Block.Rule -> HorizontalDivider(Modifier.padding(vertical = 4.dp), color = MaterialTheme.colorScheme.outline)
    }
}

/**
 * A paragraph. A displayed formula is drawn as the web draws
 * `<math display="block">`: on a line of its own, centred, between the words
 * on either side of it, which stay the paragraph they were.
 */
@Composable
private fun Paragraph(list: List<Inline>, style: TextStyle, m: Marking) {
    val runs = remember(list) { runs(list) }
    if (runs.size == 1 && runs[0] is Run.Words) {
        InlineText(list, m, style)
        return
    }
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        runs.forEach { r ->
            when (r) {
                is Run.Words -> InlineText(r.inlines, m, style)
                is Run.Shown -> DisplayMath(r.math, style)
            }
        }
    }
}

internal sealed interface Run {
    data class Words(val inlines: List<Inline>) : Run
    data class Shown(val math: Inline.Math) : Run
}

/**
 * A paragraph's inlines cut at each displayed formula that stands in it
 * directly. The space and the break on either side of a cut belong to
 * neither side, and a side that is nothing else is dropped. A displayed
 * formula inside emphasis or a link stays where it is, drawn in its line.
 */
internal fun runs(list: List<Inline>): List<Run> {
    val out = mutableListOf<Run>()
    val words = mutableListOf<Inline>()
    fun flush() {
        while (words.firstOrNull().let { it == Inline.Break || (it is Inline.Text && it.text.isBlank()) }) words.removeAt(0)
        while (words.lastOrNull().let { it == Inline.Break || (it is Inline.Text && it.text.isBlank()) }) words.removeAt(words.size - 1)
        (words.firstOrNull() as? Inline.Text)?.let { words[0] = Inline.Text(it.text.trimStart()) }
        (words.lastOrNull() as? Inline.Text)?.let { words[words.size - 1] = Inline.Text(it.text.trimEnd()) }
        if (words.isNotEmpty()) out += Run.Words(words.toList())
        words.clear()
    }
    for (i in list) {
        if (i is Inline.Math && i.display) {
            flush()
            out += Run.Shown(i)
        } else {
            words += i
        }
    }
    flush()
    return out
}

/** One block's inlines as one text, with its formulas drawn in the line. */
@Composable
private fun InlineText(list: List<Inline>, m: Marking, style: TextStyle, modifier: Modifier = Modifier) {
    val drawn = inlines(list, m, style)
    Text(drawn.text, modifier, style = style, inlineContent = drawn.formulas)
}

/** A block's text and the formulas set into it, keyed by the ids the text names them by. */
class Drawn(val text: AnnotatedString, val formulas: Map<String, InlineTextContent>)

/** The inlines of one block as one string, with a link that opens where it points. */
@Composable
fun inlines(list: List<Inline>, m: Marking = NO_MARKING, style: TextStyle = MaterialTheme.typography.bodyLarge): Drawn {
    val link = TextLinkStyles(SpanStyle(color = MaterialTheme.colorScheme.primary, textDecoration = TextDecoration.Underline))
    val codeBg = MaterialTheme.colorScheme.surfaceContainer
    val laid = rememberTypesetting(list, style)
    val density = LocalDensity.current
    val formulas = mutableMapOf<String, InlineTextContent>()
    val built = buildAnnotatedString {
        append(list, link, codeBg, m.onCite) { math ->
            val f = laid[math]
            if (f == null) {
                // What the web does with a formula it cannot read: its source, as code.
                withStyle(SpanStyle(fontFamily = Mono, background = codeBg, fontSize = 13.sp)) { append(math.tex) }
            } else {
                val id = "math:${formulas.size}"
                formulas[id] = with(density) {
                    InlineTextContent(Placeholder(f.width.toSp(), f.height.toSp(), PlaceholderVerticalAlign.AboveBaseline)) {
                        FormulaCanvas(f, Modifier.fillMaxSize())
                    }
                }
                // The source stands in for the picture wherever the text is
                // read as text: a copy out of the selection, and TalkBack.
                appendInlineContent(id, math.tex)
            }
        }
    }
    return Drawn(marked(built, m.literals, m.style), formulas)
}

/**
 * A displayed formula: centred on its own line, and scrolled sideways rather
 * than shrunk when it is wider than the screen, so a long sum stays legible.
 */
@Composable
private fun DisplayMath(math: Inline.Math, style: TextStyle) {
    val f = rememberTypesetting(listOf(math), style)[math]
    if (f == null) {
        InlineText(listOf(Inline.Code(math.tex)), NO_MARKING, style)
        return
    }
    val density = LocalDensity.current
    Box(
        Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(vertical = 4.dp),
        contentAlignment = Alignment.Center,
    ) {
        FormulaCanvas(
            f,
            with(density) { Modifier.width(f.width.toDp()).height(f.height.toDp()) }
                .semantics { contentDescription = math.tex },
            baseline = false,
        )
    }
}

/**
 * One formula, laid out by JLaTeXMath. `depth` is how far it reaches below
 * its baseline, which is what lets an inline one sit on the line of text.
 */
class Formula(val drawable: JLatexMathDrawable) {
    val width = drawable.intrinsicWidth
    val height = drawable.intrinsicHeight
    val depth = drawable.icon().iconDepth
}

/**
 * The formula drawn into its box. In a line of text the box stands on the
 * baseline, so the picture is lowered by its depth: its own baseline then
 * falls on the text's, and a subscript hangs below it as a letter's tail
 * does. On a line of its own there is no baseline to meet.
 */
@Composable
private fun FormulaCanvas(f: Formula, modifier: Modifier, baseline: Boolean = true) {
    Canvas(modifier) {
        drawIntoCanvas { c ->
            c.nativeCanvas.withSave {
                if (baseline) translate(0f, f.depth.toFloat())
                f.drawable.draw(this)
            }
        }
    }
}

/**
 * Every formula in `list`, laid out at the size and colour of the text around
 * it, or null for one JLaTeXMath cannot read. Done once per text, size and
 * colour: laying out TeX is far more work than a frame should do.
 */
@Composable
private fun rememberTypesetting(list: List<Inline>, style: TextStyle): Map<Inline.Math, Formula?> {
    val context = LocalContext.current.applicationContext
    val size = with(LocalDensity.current) { style.fontSize.takeOrElse { 16.sp }.toPx() }
    val color = style.color.takeOrElse { LocalContentColor.current }.toArgb()
    return remember(list, size, color) {
        val found = mutableListOf<Inline.Math>()
        collectMath(list, found)
        if (found.isEmpty()) emptyMap() else {
            JLatexMathAndroid.init(context)
            found.distinct().associateWith { typeset(it, size, color) }
        }
    }
}

private fun collectMath(list: List<Inline>, into: MutableList<Inline.Math>) {
    list.forEach { i ->
        when (i) {
            is Inline.Math -> into += i
            is Inline.Strong -> collectMath(i.inlines, into)
            is Inline.Emph -> collectMath(i.inlines, into)
            is Inline.Strike -> collectMath(i.inlines, into)
            is Inline.Link -> collectMath(i.inlines, into)
            else -> {}
        }
    }
}

/**
 * One formula laid out, or null where its TeX does not parse. A formula in
 * the run of a line is set in TeX's text style, as `$…$` is in TeX itself:
 * the drawable lays out everything in display style, which would stand a
 * fraction two lines tall in the middle of a sentence.
 */
internal fun typeset(math: Inline.Math, size: Float, color: Int): Formula? = try {
    Formula(
        JLatexMathDrawable.builder(if (math.display) math.tex else "\\textstyle ${math.tex}")
            .textSize(size)
            .color(color)
            .build(),
    ).takeIf { it.width > 0 && it.height > 0 }
} catch (_: Exception) {
    // JLaTeXMath refuses what it cannot read by throwing, a command it does
    // not know included. The caller shows the source instead.
    null
}

/**
 * Every occurrence of every literal, marked over the rendered text. Longest
 * first and never overlapping, so `/etc` inside `/etc/engram/config.toml` is
 * part of the longer one and not a mark within a mark. Exact matching on
 * strings the server itself lifted out of this text.
 */
fun marked(a: AnnotatedString, literals: List<String>, style: SpanStyle): AnnotatedString {
    if (literals.isEmpty()) return a
    val text = a.text
    val taken = BooleanArray(text.length)
    return buildAnnotatedString {
        append(a)
        for (lit in literals.filter { it.isNotEmpty() }.distinct().sortedByDescending { it.length }) {
            var from = text.indexOf(lit)
            while (from >= 0) {
                val to = from + lit.length
                if ((from until to).none { taken[it] }) {
                    (from until to).forEach { taken[it] = true }
                    addStyle(style, from, to)
                }
                from = text.indexOf(lit, from + 1)
            }
        }
    }
}

/**
 * Whether a link's destination may be handed to the system. The three the
 * parser's autolinks already allow, and no more: an ingested page can name
 * any scheme it likes, and `file:`, `intent:` and `javascript:` all mean
 * something to Android that a reader never asked for.
 */
private fun tappable(url: String): Boolean {
    val u = url.trimStart().lowercase()
    return u.startsWith("http://") || u.startsWith("https://") || u.startsWith("mailto:")
}

private fun AnnotatedString.Builder.append(
    list: List<Inline>,
    link: TextLinkStyles,
    codeBg: Color,
    onCite: ((Int) -> Unit)?,
    math: AnnotatedString.Builder.(Inline.Math) -> Unit,
) {
    list.forEach { i ->
        when (i) {
            is Inline.Text -> append(i.text)
            is Inline.Code -> withStyle(SpanStyle(fontFamily = Mono, background = codeBg, fontSize = 13.sp)) { append(i.text) }
            is Inline.Strong -> withStyle(SpanStyle(fontWeight = FontWeight.SemiBold)) { append(i.inlines, link, codeBg, onCite, math) }
            is Inline.Emph -> withStyle(SpanStyle(fontStyle = FontStyle.Italic)) { append(i.inlines, link, codeBg, onCite, math) }
            is Inline.Strike -> withStyle(SpanStyle(textDecoration = TextDecoration.LineThrough)) { append(i.inlines, link, codeBg, onCite, math) }
            is Inline.Link -> {
                val n = i.url.removePrefix("cite:").toIntOrNull()
                if (i.url.startsWith("cite:") && n != null && onCite != null) {
                    // A citation is a place in this screen, not an address.
                    withLink(LinkAnnotation.Clickable("cite", link) { onCite(n) }) { append(i.inlines, link, codeBg, onCite, math) }
                } else if (tappable(i.url)) {
                    withLink(LinkAnnotation.Url(i.url, link)) { append(i.inlines, link, codeBg, onCite, math) }
                } else {
                    // Anything else — `file:`, `intent:`, `javascript:` — is
                    // drawn as the words it is. The text came from a page
                    // somebody else wrote, and a tap here leaves this app.
                    append(i.inlines, link, codeBg, onCite, math)
                }
            }
            is Inline.Math -> math(i)
            Inline.Break -> append('\n')
        }
    }
}
