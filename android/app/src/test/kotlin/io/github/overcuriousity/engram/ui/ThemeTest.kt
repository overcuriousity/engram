package io.github.overcuriousity.engram.ui

import androidx.compose.material3.ColorScheme
import androidx.compose.ui.graphics.Color
import org.junit.Assert.assertEquals
import org.junit.Test
import java.io.File

class ThemeTest {
    private val css = File("../../assets/css/00-tokens.css").readText()

    private fun block(selector: String): Map<String, Color> {
        val body = css.substringAfter(selector).substringBefore('}')
        return Regex("""--([a-z0-9-]+):\s*#([0-9a-f]{6});""").findAll(body)
            .associate { it.groupValues[1] to Color(0xFF000000 or it.groupValues[2].toLong(16)) }
    }

    private fun check(t: Map<String, Color>, s: ColorScheme, muted: Color, due: Color) {
        val pairs = mapOf(
            "color-bg-base" to s.background, "color-bg-surface" to s.surface, "color-bg-elevated" to s.surfaceVariant,
            "color-bg-hover" to s.surfaceContainer, "color-bg-active" to s.surfaceContainerHigh,
            "color-fg-primary" to s.onBackground, "color-fg-secondary" to s.onSurfaceVariant, "color-fg-muted" to muted,
            "color-border" to s.outline, "color-border-subtle" to s.outlineVariant,
            "color-accent" to s.primary, "color-accent-fg" to s.onPrimary,
            "color-danger" to s.error, "color-warning" to s.secondary, "color-success" to s.tertiary, "color-due" to due,
        )
        for ((token, have) in pairs) assertEquals(token, t[token], have)
        assertEquals(s.onBackground, s.onSurface)
    }

    @Test fun palette_matches_the_manifest_tokens() {
        val due = Regex("""--color-due:\s*#([0-9a-f]{6})""").findAll(css).map { Color(0xFF000000 or it.groupValues[1].toLong(16)) }.toList()
        check(block(":root {") + ("color-due" to due.first()), EngramColors.light, EngramColors.mutedLight, EngramColors.dueLight)
        check(block(":root[data-theme=\"dark\"] {") + ("color-due" to due.last()), EngramColors.dark, EngramColors.mutedDark, EngramColors.dueDark)
    }
}
