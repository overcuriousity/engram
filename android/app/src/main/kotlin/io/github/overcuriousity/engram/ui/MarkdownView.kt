package io.github.overcuriousity.engram.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
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

/**
 * An artifact's text, drawn as the web draws it: the markdown a model wrote
 * as headings, emphasis, lists and the rest — never as its syntax. The
 * reading is `Markdown.kt`; this is only the drawing.
 */
@Composable
fun Markdown(text: String, modifier: Modifier = Modifier, style: TextStyle = MaterialTheme.typography.bodyLarge) {
    val blocks = remember(text) { parseMarkdown(text) }
    SelectionContainer(modifier) {
        Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
            blocks.forEach { BlockView(it, style) }
        }
    }
}

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
private fun BlockView(b: Block, style: TextStyle) {
    when (b) {
        is Block.Heading -> Text(
            inlines(b.inlines),
            style = when (b.level) {
                1 -> MaterialTheme.typography.titleLarge
                2 -> MaterialTheme.typography.titleMedium
                else -> MaterialTheme.typography.labelLarge
            },
            modifier = Modifier.padding(top = if (b.level <= 2) 6.dp else 2.dp),
        )
        is Block.Paragraph -> Text(inlines(b.inlines), style = style)
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
                b.blocks.forEach { BlockView(it, style.copy(color = MaterialTheme.colorScheme.onSurfaceVariant)) }
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
                        item.forEach { BlockView(it, style) }
                    }
                }
            }
        }
        is Block.Table -> Column(Modifier.fillMaxWidth()) {
            Row(Modifier.fillMaxWidth()) {
                b.header.forEach { cell ->
                    Text(inlines(cell), Modifier.weight(1f).padding(4.dp, 6.dp), style = style.copy(fontWeight = FontWeight.Medium))
                }
            }
            HorizontalDivider(color = MaterialTheme.colorScheme.outline)
            b.rows.forEach { row ->
                Row(Modifier.fillMaxWidth()) {
                    row.forEach { cell -> Text(inlines(cell), Modifier.weight(1f).padding(4.dp, 6.dp), style = style) }
                }
                HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
            }
        }
        Block.Rule -> HorizontalDivider(Modifier.padding(vertical = 4.dp), color = MaterialTheme.colorScheme.outline)
    }
}

/** The inlines of one block as one string, with a link that opens where it points. */
@Composable
fun inlines(list: List<Inline>): AnnotatedString {
    val link = TextLinkStyles(SpanStyle(color = MaterialTheme.colorScheme.primary, textDecoration = TextDecoration.Underline))
    val codeBg = MaterialTheme.colorScheme.surfaceContainer
    return buildAnnotatedString { append(list, link, codeBg) }
}

private fun AnnotatedString.Builder.append(list: List<Inline>, link: TextLinkStyles, codeBg: Color) {
    list.forEach { i ->
        when (i) {
            is Inline.Text -> append(i.text)
            is Inline.Code -> withStyle(SpanStyle(fontFamily = Mono, background = codeBg, fontSize = 13.sp)) { append(i.text) }
            is Inline.Strong -> withStyle(SpanStyle(fontWeight = FontWeight.SemiBold)) { append(i.inlines, link, codeBg) }
            is Inline.Emph -> withStyle(SpanStyle(fontStyle = FontStyle.Italic)) { append(i.inlines, link, codeBg) }
            is Inline.Strike -> withStyle(SpanStyle(textDecoration = TextDecoration.LineThrough)) { append(i.inlines, link, codeBg) }
            is Inline.Link -> withLink(LinkAnnotation.Url(i.url, link)) { append(i.inlines, link, codeBg) }
            Inline.Break -> append('\n')
        }
    }
}
