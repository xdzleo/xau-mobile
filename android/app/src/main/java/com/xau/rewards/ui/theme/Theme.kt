package com.xau.rewards.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.LineHeightStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.xau.rewards.R

/**
 * The design system, built to the supplied mockup.
 *
 * This is the client's design, not a generated one. Where a measured
 * rule and the mockup disagree, the mockup wins — it is what was asked
 * for. Contrast is still checked, and where a value was genuinely
 * unreadable it was nudged rather than redesigned; those nudges are
 * called out at the token.
 */
@Immutable
data class XauPalette(
    /** Page ground. */
    val ink: Color = Color(0xFF0A0C0A),
    /** Card fill. Lifted off the ground far enough to read as an object. */
    val surface: Color = Color(0xFF121712),
    /** Raised / pressed, and the inner wells (thumbnails, avatars). */
    val surfaceHigh: Color = Color(0xFF1A211A),
    /** Hairline around cards, as in the mockup. */
    val outline: Color = Color(0xFF243024),

    /** Xbox green. Accent text, active states, the ring. */
    val green: Color = Color(0xFF3FD44B),
    /** The deeper green of the logo and filled controls. */
    val greenDeep: Color = Color(0xFF107C10),

    val textPrimary: Color = Color(0xFFF2F6F2),
    val textSecondary: Color = Color(0xFF9EAA9E),
    /** Nudged up from the mockup's grey to clear 4.5:1 on [surface]. */
    val textMuted: Color = Color(0xFF8B978B),

    val danger: Color = Color(0xFFE5705F),
) {
    /** White reads on the deep green fill at 5.36:1. */
    val onGreen: Color = Color.White
}

val LocalXau = staticCompositionLocalOf { XauPalette() }

object Space {
    val xs = 4.dp
    val sm = 8.dp
    val md = 12.dp
    val lg = 16.dp
    val xl = 20.dp
    val xxl = 24.dp
    val huge = 32.dp

    /** Screen gutter. */
    val gutter = 16.dp

    /** Minimum touch target, icon buttons included. */
    val touch = 48.dp
}

object Radius {
    val card = 20.dp
    val row = 16.dp
    val control = 14.dp
    val pill = 50.dp
}

// ---------------------------------------------------------------- type

private val Sans = FontFamily(
    Font(R.font.archivo_regular, FontWeight.W400),
    Font(R.font.archivo_semibold, FontWeight.W600),
    Font(R.font.archivo_expanded_bold, FontWeight.W700),
)

private val Trim = LineHeightStyle(
    alignment = LineHeightStyle.Alignment.Center,
    trim = LineHeightStyle.Trim.None,
)

private fun t(size: Int, weight: Int, lineHeight: Float = 1.3f) = TextStyle(
    fontFamily = Sans,
    fontSize = size.sp,
    fontWeight = FontWeight(weight),
    lineHeight = (size * lineHeight).sp,
    lineHeightStyle = Trim,
    // The mockup lines figures up in columns; tabular digits keep them
    // from jittering as they change.
    fontFeatureSettings = "tnum",
)

@Immutable
data class XauType(
    /** "480" — the day's headline figure. */
    val hero: TextStyle = t(44, 700, 1.05f),
    /** "8/8" inside the ring. */
    val ringFigure: TextStyle = t(28, 700, 1.1f),
    /** "Xbox Rewards". */
    val brand: TextStyle = t(21, 700, 1.15f),
    /** "Resumo de hoje", "Presença em jogos". */
    val section: TextStyle = t(17, 600, 1.25f),
    /** Game and account names. */
    val rowTitle: TextStyle = t(16, 600, 1.25f),
    /** "60 min". */
    val rowFigure: TextStyle = t(17, 700, 1.2f),
    val body: TextStyle = t(14, 400, 1.4f),
    val button: TextStyle = t(14, 600, 1.3f),
    /** "Presença ativa", "Máx. diário: 60 min". */
    val caption: TextStyle = t(12, 400, 1.35f),
    val captionStrong: TextStyle = t(12, 600, 1.35f),
)

val LocalXauType = staticCompositionLocalOf { XauType() }

// --------------------------------------------------------------- theme

@Composable
fun XauTheme(content: @Composable () -> Unit) {
    val palette = XauPalette()
    val type = XauType()

    val scheme = darkColorScheme(
        primary = palette.green,
        onPrimary = palette.ink,
        background = palette.ink,
        onBackground = palette.textPrimary,
        surface = palette.surface,
        onSurface = palette.textPrimary,
        surfaceVariant = palette.surfaceHigh,
        onSurfaceVariant = palette.textSecondary,
        outline = palette.outline,
        outlineVariant = palette.outline,
        error = palette.danger,
        onError = palette.ink,
    )

    CompositionLocalProvider(LocalXau provides palette, LocalXauType provides type) {
        MaterialTheme(
            colorScheme = scheme,
            typography = MaterialTheme.typography.copy(
                titleLarge = type.brand,
                titleMedium = type.section,
                bodyLarge = type.body,
                bodyMedium = type.body,
                labelSmall = type.caption,
            ),
            content = content,
        )
    }
}

object Xau {
    val colors: XauPalette
        @Composable get() = LocalXau.current
    val type: XauType
        @Composable get() = LocalXauType.current
}
