package com.xau.rewards.ui.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontVariation
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.LineHeightStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import androidx.compose.ui.unit.sp
import com.xau.rewards.R

/**
 * The design system, as code.
 *
 * The rules this encodes live in `DESIGN.md` at the repo root. Two of
 * them are enforced by the shape of this file rather than by discipline:
 *
 * - **Green is not text.** [XauPalette.brand] is a fill. On the ground
 *   colour it measures 3.30:1, which fails WCAG AA for text, so it is
 *   never offered as a content colour — [XauPalette.brandBright] (7.68:1)
 *   exists for the cases that want green *type*.
 * - **Spacing has one scale.** [Space] is the only source of gaps; a
 *   literal `12.dp` in a screen is a bug, because inconsistent density
 *   is the loudest signal of a cheap app.
 */
@Immutable
data class XauPalette(
    /** Ground. Near-black with a green bias — never a neutral grey. */
    val ink: Color = Color(0xFF0B0F0D),
    val surface: Color = Color(0xFF131A16),
    val surfaceHigh: Color = Color(0xFF1B231E),
    val outline: Color = Color(0xFF26312A),

    /** Xbox green. Fill only — one element per screen. */
    val brand: Color = Color(0xFF107C10),
    /** The green that is legible as type: 7.68:1 on [surface]. */
    val brandBright: Color = Color(0xFF16C60C),

    /**
     * Points waiting to be claimed. Deliberately not the brand green:
     * if "there is something here" wore the same colour as "this is a
     * button", neither would mean anything.
     */
    val waiting: Color = Color(0xFFF0A02C),
    val danger: Color = Color(0xFFE5705F),

    val textPrimary: Color = Color(0xFFE8EFE9),
    val textSecondary: Color = Color(0xFFA8B3AC),
    /** 4.78:1 on [surface] — the floor, not a suggestion. */
    val textMuted: Color = Color(0xFF7B8880),
)

val LocalXau = staticCompositionLocalOf { XauPalette() }

/** The one spacing scale. Nothing outside it. */
object Space {
    val xs = 4.dp
    val sm = 8.dp
    val md = 12.dp
    val lg = 16.dp
    val xl = 24.dp
    val xxl = 32.dp

    /** Screen gutter. */
    val gutter = 16.dp

    /** Minimum touch target — applies to icon buttons too. */
    val touch = 48.dp
}

object Radius {
    val card = 20.dp
    val control = 16.dp
    val chip = 12.dp
}

// ---------------------------------------------------------------- type

/**
 * Inter, as a variable font.
 *
 * Roboto is banned by `DESIGN.md`: it is the Android default and reads
 * as unstyled. One variable file covers every weight in the scale,
 * including the 650 that no static weight would give us.
 */
@OptIn(androidx.compose.ui.text.ExperimentalTextApi::class)
private fun inter(weight: Int) = Font(
    R.font.inter_variable,
    weight = FontWeight(weight),
    variationSettings = FontVariation.Settings(FontVariation.weight(weight)),
)

private val Inter = FontFamily(
    inter(400), inter(600), inter(650), inter(700), inter(800),
)

/** Keeps line boxes from adding stray leading above short headings. */
private val Trim = LineHeightStyle(
    alignment = LineHeightStyle.Alignment.Center,
    trim = LineHeightStyle.Trim.None,
)

private fun style(
    size: Int,
    weight: Int,
    tracking: Float = 0f,
    lineHeight: Float = 1.35f,
) = TextStyle(
    fontFamily = Inter,
    fontSize = size.sp,
    fontWeight = FontWeight(weight),
    // Tabular figures everywhere. This screen is almost entirely
    // numbers, and proportional digits make a balance jitter sideways
    // as it counts up.
    fontFeatureSettings = "tnum",
    // Tracking in em rather than sp, so it scales with the type instead
    // of drifting when the user raises their font size.
    letterSpacing = tracking.em,
    lineHeight = (size * lineHeight).sp,
    lineHeightStyle = Trim,
)

/**
 * The type scale from `DESIGN.md` §4. Negative tracking on large text is
 * what makes a big number look *designed* rather than merely large;
 * positive tracking on small uppercase labels keeps them from reading
 * as a mistake.
 */
@Immutable
data class XauType(
    val hero: TextStyle = style(56, 800, -0.02f, 1.05f),
    val screenTitle: TextStyle = style(24, 700, -0.02f, 1.2f),
    val figure: TextStyle = style(32, 700, -0.02f, 1.1f),
    val section: TextStyle = style(17, 650, -0.01f, 1.3f),
    val body: TextStyle = style(15, 400),
    val rowTitle: TextStyle = style(15, 600, 0f, 1.3f),
    val secondary: TextStyle = style(13, 400, 0f, 1.4f),
    val caption: TextStyle = style(11, 400, 0f, 1.4f),
    val label: TextStyle = style(11, 600, 0.14f, 1.4f),
)

val LocalXauType = staticCompositionLocalOf { XauType() }

// --------------------------------------------------------------- theme

@Composable
fun XauTheme(content: @Composable () -> Unit) {
    val palette = XauPalette()
    val type = XauType()

    // Dark only — `DESIGN.md` §5. This app is used at night, next to a
    // console, and a light mode nobody asked for is a second design to
    // keep correct rather than a feature. The system preference is
    // deliberately not consulted.
    val scheme = darkColorScheme(
        primary = palette.brand,
        // On brand green, white measures 5.36:1 — the only pairing that
        // clears AA on that fill.
        onPrimary = Color.White,
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
            typography = Typography(
                titleLarge = type.screenTitle,
                titleMedium = type.section,
                bodyLarge = type.body,
                bodyMedium = type.secondary,
                labelSmall = type.label,
            ),
            content = content,
        )
    }
}

/** Shorthand so screens read `Xau.colors.waiting` rather than the local. */
object Xau {
    val colors: XauPalette
        @Composable get() = LocalXau.current
    val type: XauType
        @Composable get() = LocalXauType.current
}
