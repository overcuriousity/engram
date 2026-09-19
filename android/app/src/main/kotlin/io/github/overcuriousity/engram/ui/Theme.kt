package io.github.overcuriousity.engram.ui

import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.github.overcuriousity.engram.R

/** assets/css/00-tokens.css, copied — not approximated. */
object EngramColors {
    val light = lightColorScheme(
        background = Color(0xFFF8F6F1), surface = Color(0xFFF2F0EA), surfaceVariant = Color(0xFFFFFFFF),
        surfaceContainer = Color(0xFFECE9E2), surfaceContainerHigh = Color(0xFFE2DED3),
        onBackground = Color(0xFF2D2D2D), onSurface = Color(0xFF2D2D2D), onSurfaceVariant = Color(0xFF5A5A5A),
        outline = Color(0xFFDDD8CC), outlineVariant = Color(0xFFEAE7DE),
        primary = Color(0xFF386889), onPrimary = Color(0xFFFFFFFF), primaryContainer = Color(0x1A386889),
        error = Color(0xFFB3382C), tertiary = Color(0xFF2B7048), secondary = Color(0xFF845B16),
    )
    val dark = darkColorScheme(
        background = Color(0xFF0E1015), surface = Color(0xFF14171D), surfaceVariant = Color(0xFF1B1E26),
        surfaceContainer = Color(0xFF262A34), surfaceContainerHigh = Color(0xFF2D3140),
        onBackground = Color(0xFFE2E4EC), onSurface = Color(0xFFE2E4EC), onSurfaceVariant = Color(0xFF9599B0),
        outline = Color(0xFF232636), outlineVariant = Color(0xFF191C26),
        primary = Color(0xFF5AA8B0), onPrimary = Color(0xFF0E1015), primaryContainer = Color(0x265AA8B0),
        error = Color(0xFFE77676), tertiary = Color(0xFF4CAF7D), secondary = Color(0xFFE8A839),
    )

    /** `--color-fg-muted` and `--color-due`, which Material has no slot for. */
    val mutedLight = Color(0xFF6C6C65)
    val mutedDark = Color(0xFF8185A3)
    val dueLight = Color(0xFFA15A1A)
    val dueDark = Color(0xFFE0A060)
}

val Inter = FontFamily(
    Font(R.font.inter_400, FontWeight.Normal),
    Font(R.font.inter_500, FontWeight.Medium),
    Font(R.font.inter_600, FontWeight.SemiBold),
)
val Mono = FontFamily(Font(R.font.jetbrains_mono_400, FontWeight.Normal))

/** The type scale: 0.75, 0.8125, 0.875, 0.9375, 1.125, 1.375, 1.75 rem at 16 px. */
val EngramType = Typography(
    labelSmall = TextStyle(fontFamily = Inter, fontSize = 12.sp),
    bodySmall = TextStyle(fontFamily = Inter, fontSize = 13.sp),
    bodyMedium = TextStyle(fontFamily = Inter, fontSize = 14.sp),
    bodyLarge = TextStyle(fontFamily = Inter, fontSize = 15.sp),
    titleMedium = TextStyle(fontFamily = Inter, fontSize = 18.sp, fontWeight = FontWeight.Medium),
    titleLarge = TextStyle(fontFamily = Inter, fontSize = 22.sp, fontWeight = FontWeight.SemiBold),
    headlineMedium = TextStyle(fontFamily = Inter, fontSize = 28.sp, fontWeight = FontWeight.SemiBold),
    labelMedium = TextStyle(fontFamily = Mono, fontSize = 13.sp),
    labelLarge = TextStyle(fontFamily = Inter, fontSize = 14.sp, fontWeight = FontWeight.Medium),
)

val EngramShapes = Shapes(
    small = RoundedCornerShape(3.dp),
    medium = RoundedCornerShape(6.dp),
    large = RoundedCornerShape(6.dp),
)

/** Whether the scheme in force is the dark one — the chosen theme, not the system's. */
@Composable
private fun isDark(): Boolean = MaterialTheme.colorScheme.background == EngramColors.dark.background

@Composable
fun muted(): Color = if (isDark()) EngramColors.mutedDark else EngramColors.mutedLight

@Composable
fun due(): Color = if (isDark()) EngramColors.dueDark else EngramColors.dueLight

/** The web's toggle: follow the system, or one of the two on purpose. */
enum class ThemeMode { System, Light, Dark }

@Composable
fun EngramTheme(mode: ThemeMode = ThemeMode.System, content: @Composable () -> Unit) {
    val dark = when (mode) {
        ThemeMode.System -> isSystemInDarkTheme()
        ThemeMode.Light -> false
        ThemeMode.Dark -> true
    }
    // The bars' icons follow the system unless told, and a light app on a dark
    // phone then has white icons on its light background.
    val activity = androidx.activity.compose.LocalActivity.current as? androidx.activity.ComponentActivity
    androidx.compose.runtime.LaunchedEffect(dark) {
        val style = androidx.activity.SystemBarStyle.auto(android.graphics.Color.TRANSPARENT, android.graphics.Color.TRANSPARENT) { dark }
        activity?.enableEdgeToEdge(style, style)
    }
    MaterialTheme(
        colorScheme = if (dark) EngramColors.dark else EngramColors.light,
        typography = EngramType,
        shapes = EngramShapes,
        content = content,
    )
}
