package io.github.overcuriousity.engram.ui

/*
 * Markdown, read into a tree the screen can draw. Pure: no Android in it, so
 * it runs under a plain JUnit test and so the rule about what a `#` means is
 * in one place and testable without a device.
 *
 * The server renders an artifact with pulldown-cmark and ammonia — headings,
 * emphasis, code, lists, quotes, rules, tables, strikethrough, links — and
 * this reads the same dialect. Where the two disagree the web is right and
 * this is the bug; nothing here is a house style. What it deliberately does
 * not do is render a passage: a slice of a document is kept as it was written
 * (see `artifact_html` on the server), and the screen draws that verbatim.
 */

sealed interface Block {
    data class Heading(val level: Int, val inlines: List<Inline>) : Block
    data class Paragraph(val inlines: List<Inline>) : Block
    data class Code(val text: String, val lang: String? = null) : Block
    data class Quote(val blocks: List<Block>) : Block
    data class ListBlock(val ordered: Boolean, val start: Int, val items: List<List<Block>>) : Block
    data class Table(val header: List<List<Inline>>, val rows: List<List<List<Inline>>>) : Block
    data object Rule : Block
}

sealed interface Inline {
    data class Text(val text: String) : Inline
    data class Code(val text: String) : Inline
    data class Strong(val inlines: List<Inline>) : Inline
    data class Emph(val inlines: List<Inline>) : Inline
    data class Strike(val inlines: List<Inline>) : Inline
    data class Link(val inlines: List<Inline>, val url: String) : Inline
    data object Break : Inline
}

/** The tree of `src`. Never throws: unreadable markup is text. */
fun parseMarkdown(src: String): List<Block> = Blocks(src.replace("\r\n", "\n").replace('\r', '\n').lines()).parse()

/**
 * The words alone, on one line — what a result row shows under its name. The
 * same reading the server's `markdown::snippet` does: text and code kept,
 * every break and every block end a space, markup dropped.
 */
fun plain(markdown: String): String {
    val out = StringBuilder()
    fun inlines(list: List<Inline>) {
        list.forEach { i ->
            when (i) {
                is Inline.Text -> out.append(i.text)
                is Inline.Code -> out.append(i.text)
                is Inline.Strong -> inlines(i.inlines)
                is Inline.Emph -> inlines(i.inlines)
                is Inline.Strike -> inlines(i.inlines)
                is Inline.Link -> inlines(i.inlines)
                Inline.Break -> out.append(' ')
            }
        }
        out.append(' ')
    }
    fun blocks(list: List<Block>) {
        list.forEach { b ->
            when (b) {
                is Block.Heading -> inlines(b.inlines)
                is Block.Paragraph -> inlines(b.inlines)
                is Block.Code -> out.append(b.text).append(' ')
                is Block.Quote -> blocks(b.blocks)
                is Block.ListBlock -> b.items.forEach { blocks(it) }
                is Block.Table -> { b.header.forEach { inlines(it) }; b.rows.forEach { r -> r.forEach { inlines(it) } } }
                Block.Rule -> out.append(' ')
            }
        }
    }
    blocks(parseMarkdown(markdown))
    return out.toString().replace(Regex("\\s+"), " ").trim()
}

// ── Blocks ───────────────────────────────────────────────────────────────

private val HEADING = Regex("^ {0,3}(#{1,6})(?:[ \\t]+(.*?))?[ \\t]*#*[ \\t]*$")
private val FENCE = Regex("^( {0,3})(`{3,}|~{3,})[ \\t]*([^`\\s]*)[ \\t]*$")
private val RULE = Regex("^ {0,3}([-*_])[ \\t]*(?:\\1[ \\t]*){2,}$")
private val BULLET = Regex("^( *)([-*+])( +)(.*)$")
private val NUMBER = Regex("^( *)(\\d{1,9})([.)])( +)(.*)$")
private val QUOTE = Regex("^ {0,3}> ?(.*)$")
private val TABLE_DELIM = Regex("^\\s*\\|?\\s*:?-+:?\\s*(\\|\\s*:?-+:?\\s*)*\\|?\\s*$")
private val SETEXT = Regex("^ {0,3}(=+|-+)[ \\t]*$")

private class Blocks(private val lines: List<String>) {
    private var at = 0
    private val out = mutableListOf<Block>()
    private val para = mutableListOf<String>()

    fun parse(): List<Block> {
        while (at < lines.size) {
            val line = lines[at]
            when {
                line.isBlank() -> { flush(); at++ }
                fence(line) -> {}
                heading(line) -> {}
                setext(line) -> {}
                RULE.matches(line) -> { flush(); out += Block.Rule; at++ }
                quote(line) -> {}
                list(line) -> {}
                table(line) -> {}
                else -> { para += line; at++ }
            }
        }
        flush()
        return out
    }

    private fun flush() {
        if (para.isEmpty()) return
        out += Block.Paragraph(paragraphInlines(para))
        para.clear()
    }

    private fun heading(line: String): Boolean {
        val m = HEADING.matchEntire(line) ?: return false
        flush()
        out += Block.Heading(m.groupValues[1].length, parseInlines(m.groupValues[2].trim()))
        at++
        return true
    }

    /** A line of `=` or `-` directly under a paragraph makes it a heading. */
    private fun setext(line: String): Boolean {
        if (para.isEmpty()) return false
        val m = SETEXT.matchEntire(line) ?: return false
        val level = if (m.groupValues[1][0] == '=') 1 else 2
        out += Block.Heading(level, paragraphInlines(para))
        para.clear()
        at++
        return true
    }

    private fun fence(line: String): Boolean {
        val m = FENCE.matchEntire(line) ?: return false
        flush()
        val indent = m.groupValues[1].length
        val marker = m.groupValues[2]
        val lang = m.groupValues[3].ifEmpty { null }
        at++
        val body = mutableListOf<String>()
        while (at < lines.size) {
            val l = lines[at]
            val close = FENCE.matchEntire(l)
            if (close != null && close.groupValues[2][0] == marker[0] && close.groupValues[2].length >= marker.length && close.groupValues[3].isEmpty()) {
                at++
                break
            }
            body += l.drop(minOf(indent, l.takeWhile { it == ' ' }.length))
            at++
        }
        out += Block.Code(body.joinToString("\n"), lang)
        return true
    }

    private fun quote(line: String): Boolean {
        if (QUOTE.matchEntire(line) == null) return false
        flush()
        val inner = mutableListOf<String>()
        while (at < lines.size) {
            val l = lines[at]
            val m = QUOTE.matchEntire(l)
            when {
                m != null -> inner += m.groupValues[1]
                // A paragraph continues into a quote without the marker.
                l.isNotBlank() && inner.isNotEmpty() && inner.last().isNotBlank() && !opensBlock(l) -> inner += l
                else -> break
            }
            at++
        }
        out += Block.Quote(Blocks(inner).parse())
        return true
    }

    /** What a lazy continuation line must not be: the opening of anything else. */
    private fun opensBlock(l: String) =
        HEADING.matches(l) || FENCE.matches(l) || RULE.matches(l) || BULLET.matches(l) || NUMBER.matches(l) || QUOTE.matches(l)

    private class Item(val indent: Int, val width: Int, val lines: MutableList<String>)

    private fun list(line: String): Boolean {
        val first = marker(line) ?: return false
        // `* * *` is a rule, and `- ` followed by nothing is an empty item.
        flush()
        val ordered = first.ordered
        val start = first.number
        val indent = first.indent
        val items = mutableListOf<Item>()
        while (at < lines.size) {
            val l = lines[at]
            val m = marker(l)
            val current = items.lastOrNull()
            when {
                // A new item of this list: same indent, same kind of marker.
                m != null && m.indent == indent && m.ordered == ordered -> {
                    items += Item(indent, m.width, mutableListOf(m.rest))
                }
                current == null -> break
                l.isBlank() -> {
                    // Blank inside an item is kept; two blanks in a row, or a
                    // blank before something not indented, ends the list.
                    val next = lines.getOrNull(at + 1)
                    if (next == null || next.isBlank() || leading(next) < indent + current.width) {
                        if (next != null && !next.isBlank() && marker(next)?.let { it.indent == indent && it.ordered == ordered } == true) {
                            current.lines += ""
                        } else {
                            break
                        }
                    } else {
                        current.lines += ""
                    }
                }
                leading(l) >= indent + current.width -> current.lines += l.drop(indent + current.width)
                // A line less indented than the content but not blank and not
                // a marker continues the item's paragraph, as CommonMark's
                // lazy continuation does.
                current.lines.last().isNotBlank() && (m == null || m.indent > indent) && !opensBlock(l.trim()) ->
                    current.lines += l.trim()
                else -> break
            }
            at++
        }
        out += Block.ListBlock(ordered, start, items.map { Blocks(it.lines).parse() })
        return true
    }

    private class Marker(val indent: Int, val width: Int, val ordered: Boolean, val number: Int, val rest: String)

    private fun marker(l: String): Marker? {
        if (RULE.matches(l)) return null
        BULLET.matchEntire(l)?.let { m ->
            val indent = m.groupValues[1].length
            if (indent > 8) return null
            val spaces = m.groupValues[3].length
            // Five or more spaces after the marker is one space and an indented line.
            val width = 1 + if (spaces >= 5) 1 else spaces
            val rest = if (spaces >= 5) " ".repeat(spaces - 1) + m.groupValues[4] else m.groupValues[4]
            return Marker(indent, width, false, 1, rest)
        }
        NUMBER.matchEntire(l)?.let { m ->
            val indent = m.groupValues[1].length
            if (indent > 8) return null
            val spaces = m.groupValues[4].length
            val width = m.groupValues[2].length + 1 + if (spaces >= 5) 1 else spaces
            val rest = if (spaces >= 5) " ".repeat(spaces - 1) + m.groupValues[5] else m.groupValues[5]
            return Marker(indent, width, true, m.groupValues[2].toIntOrNull() ?: 1, rest)
        }
        return null
    }

    private fun leading(l: String) = l.takeWhile { it == ' ' }.length

    private fun table(line: String): Boolean {
        if (!line.contains('|')) return false
        val delim = lines.getOrNull(at + 1) ?: return false
        if (!TABLE_DELIM.matches(delim) || !delim.contains('-')) return false
        val header = cells(line)
        val width = cells(delim).size
        if (header.size != width) return false
        flush()
        at += 2
        val rows = mutableListOf<List<List<Inline>>>()
        while (at < lines.size) {
            val l = lines[at]
            if (l.isBlank() || !l.contains('|')) break
            val c = cells(l)
            rows += List(width) { i -> c.getOrNull(i) ?: emptyList() }
            at++
        }
        out += Block.Table(header, rows)
        return true
    }

    /** The cells of a row, split on unescaped pipes outside code spans. */
    private fun cells(row: String): List<List<Inline>> {
        var s = row.trim()
        if (s.startsWith("|")) s = s.drop(1)
        if (s.endsWith("|") && !s.endsWith("\\|")) s = s.dropLast(1)
        val parts = mutableListOf<String>()
        val cur = StringBuilder()
        var inCode = false
        var i = 0
        while (i < s.length) {
            val c = s[i]
            when {
                c == '\\' && i + 1 < s.length && s[i + 1] == '|' -> { cur.append('|'); i++ }
                c == '`' -> { inCode = !inCode; cur.append(c) }
                c == '|' && !inCode -> { parts += cur.toString(); cur.setLength(0) }
                else -> cur.append(c)
            }
            i++
        }
        parts += cur.toString()
        return parts.map { parseInlines(it.trim()) }
    }
}

/** The lines of a paragraph, joined: two trailing spaces or a backslash make a hard break. */
private fun paragraphInlines(lines: List<String>): List<Inline> {
    val out = mutableListOf<Inline>()
    lines.forEachIndexed { i, raw ->
        val hard = raw.endsWith("  ") || raw.endsWith("\\")
        val text = if (raw.endsWith("\\")) raw.dropLast(1) else raw.trimEnd()
        val parsed = parseInlines(text.trimStart())
        if (i > 0) {
            // A soft break is a space, as the web's flow is.
            if (out.lastOrNull() != Inline.Break) out += Inline.Text(" ")
        }
        out += parsed
        if (hard && i < lines.size - 1) out += Inline.Break
    }
    return merge(out)
}

// ── Inlines ──────────────────────────────────────────────────────────────

private val URL = Regex("https?://[^\\s<>()\\[\\]]+")

fun parseInlines(src: String): List<Inline> = merge(Inlines(src).parse())

/** Adjacent texts as one, so a renderer sees words rather than characters. */
private fun merge(list: List<Inline>): List<Inline> {
    val out = mutableListOf<Inline>()
    for (i in list) {
        val last = out.lastOrNull()
        if (i is Inline.Text && last is Inline.Text) out[out.size - 1] = Inline.Text(last.text + i.text)
        else if (!(i is Inline.Text && i.text.isEmpty())) out += i
    }
    return out
}

private class Inlines(private val s: String) {
    private var i = 0

    fun parse(until: String? = null): List<Inline> {
        val out = mutableListOf<Inline>()
        val text = StringBuilder()
        fun flush() { if (text.isNotEmpty()) { out += Inline.Text(text.toString()); text.setLength(0) } }
        while (i < s.length) {
            if (until != null && s.startsWith(until, i) && (until != "_" || !wordChar(i + 1))) {
                flush()
                return out
            }
            val c = s[i]
            when {
                c == '\\' && i + 1 < s.length && s[i + 1] in PUNCT -> { text.append(s[i + 1]); i += 2 }
                c == '`' -> { flush(); out += code() ?: run { text.append('`'); i++; Inline.Text("") } }
                c == '*' || c == '_' -> {
                    val double = i + 1 < s.length && s[i + 1] == c
                    val d = if (double) "$c$c" else "$c"
                    val inner = span(d)
                    if (inner != null) { flush(); out += if (double) Inline.Strong(inner) else Inline.Emph(inner) }
                    else { text.append(d); i += d.length }
                }
                c == '~' && s.startsWith("~~", i) -> {
                    val inner = span("~~")
                    if (inner != null) { flush(); out += Inline.Strike(inner) } else { text.append("~~"); i += 2 }
                }
                c == '!' && s.startsWith("![", i) -> {
                    i++
                    val l = link()
                    if (l != null) { flush(); out += l } else text.append('!')
                }
                c == '[' -> { val l = link(); if (l != null) { flush(); out += l } else { text.append('['); i++ } }
                c == '<' -> { val a = autolink(); if (a != null) { flush(); out += a } else { text.append('<'); i++ } }
                c == 'h' && (s.startsWith("http://", i) || s.startsWith("https://", i)) -> {
                    val m = URL.matchAt(s, i)
                    if (m != null && (i == 0 || !wordChar(i - 1))) {
                        var url = m.value.trimEnd('.', ',', ';', ':', '!', '?', '"', '\'')
                        flush(); out += Inline.Link(listOf(Inline.Text(url)), url); i += url.length
                    } else { text.append(c); i++ }
                }
                else -> { text.append(c); i++ }
            }
        }
        flush()
        // Reached the end inside a delimited span: its closer was never found.
        if (until != null) unclosed = true
        return out
    }

    /** Set when a delimited span reached the end without its closer. */
    private var unclosed = false

    private fun wordChar(at: Int) = at in s.indices && (s[at].isLetterOrDigit())

    /** `*…*`, `**…**`, `~~…~~`: the inner inlines up to the matching closer, or null where there is none. */
    private fun span(d: String): List<Inline>? {
        val start = i
        val after = i + d.length
        // A run that opens onto a space is not an opener: `a * b`.
        if (after >= s.length || s[after].isWhitespace()) return null
        if (d == "_" && start > 0 && wordChar(start - 1)) return null
        i = after
        unclosed = false
        val inner = parse(d)
        if (unclosed || i >= s.length || !s.startsWith(d, i) || s[i - 1].isWhitespace()) {
            i = start
            unclosed = false
            return null
        }
        i += d.length
        return inner
    }

    private fun code(): Inline? {
        var n = 0
        while (i + n < s.length && s[i + n] == '`') n++
        val open = "`".repeat(n)
        val close = s.indexOf(open, i + n)
        // Find a closing run of exactly n backticks.
        var at = close
        while (at >= 0) {
            var m = 0
            while (at + m < s.length && s[at + m] == '`') m++
            if (m == n) break
            at = s.indexOf(open, at + m)
        }
        if (at < 0) return null
        var body = s.substring(i + n, at)
        if (body.length >= 2 && body.startsWith(" ") && body.endsWith(" ") && body.isNotBlank()) body = body.substring(1, body.length - 1)
        i = at + n
        return Inline.Code(body.replace('\n', ' '))
    }

    private fun link(): Inline? {
        // i is at '['
        var depth = 0
        var j = i
        while (j < s.length) {
            when (s[j]) {
                '\\' -> j++
                '[' -> depth++
                ']' -> { depth--; if (depth == 0) break }
            }
            j++
        }
        if (j >= s.length || j + 1 >= s.length || s[j + 1] != '(') return null
        val labelText = s.substring(i + 1, j)
        var k = j + 2
        var paren = 0
        val urlStart = k
        while (k < s.length) {
            val c = s[k]
            if (c == '\\') { k += 2; continue }
            if (c == '(') paren++
            if (c == ')') { if (paren == 0) break; paren-- }
            k++
        }
        if (k >= s.length) return null
        var url = s.substring(urlStart, k).trim()
        // A title after the destination: `(url "title")`.
        val sp = url.indexOf(' ')
        if (sp > 0) url = url.substring(0, sp)
        if (url.startsWith("<") && url.endsWith(">")) url = url.substring(1, url.length - 1)
        i = k + 1
        val label = parseInlines(labelText)
        return Inline.Link(if (label.isEmpty()) listOf(Inline.Text(url)) else label, url)
    }

    private fun autolink(): Inline? {
        val end = s.indexOf('>', i)
        if (end < 0) return null
        val body = s.substring(i + 1, end)
        if (!(body.startsWith("http://") || body.startsWith("https://") || body.startsWith("mailto:")) || body.any { it.isWhitespace() }) return null
        i = end + 1
        return Inline.Link(listOf(Inline.Text(body)), body)
    }

    private companion object {
        const val PUNCT = "\\`*_{}[]()#+-.!|~<>\"'"
    }
}
