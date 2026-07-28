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
import androidx.compose.ui.unit.em
import androidx.compose.ui.unit.sp
import com.xau.rewards.R

/**
 * The design system — the "Guide" direction.
 *
 * See `docs/GUIDE-spec.md`. It replaces a first attempt that an audit
 * measured and found to be the AI-default cluster: a neutral near-black
 * ramp with one accent, a 56sp balance as hero, rounded cards in a
 * stack, and Inter. The numbers behind that verdict are worth keeping
 * visible, because they are why each choice below is what it is:
 *
 *   old ink `#0B0F0D`      1.6% channel spread — a neutral grey with a
 *                          comment claiming a green bias
 *   card on ground         1.09:1 — which is why every container needed
 *                          a 1dp border to exist at all
 *   selected account       1.10:1 — the most important affordance in a
 *                          multi-account app, effectively invisible
 *   textMuted on surfaceHigh  4.35:1 — a real AA failure, on the
 *                          selected row specifically
 *
 * Three rules are now enforced by construction rather than by care:
 * the text floor is measured against the LIGHTEST surface, so the
 * 4.35:1 failure cannot recur; separation is a value step big enough to
 * need no border; and `#107C10` is absent from the UI entirely.
 */
@Immutable
data class XauPalette(
    // ---- surfaces ----
    /** Ground. A blue-cast graphite: 8.6% channel spread, not a grey. */
    val carbon: Color = Color(0xFF0A1220),
    /** A block. 1.65:1 on [carbon] — an object with NO border. */
    val slab: Color = Color(0xFF2E3B4D),
    /** Raised/pressed. 2.08:1. */
    val slabHigh: Color = Color(0xFF3A4A60),
    /** A line weight. Never a fill. */
    val rule: Color = Color(0xFF465873),

    // ---- text ----
    /** Primary text, and the focus rectangle. 8.08:1 on [slabHigh]. */
    val focus: Color = Color(0xFFEEF3F8),
    val dim: Color = Color(0xFFC2CEDC),
    /** Registers, IDs, units. The floor: 4.58:1 on [slabHigh]. */
    val muted: Color = Color(0xFFADBACB),

    // ---- signal ----
    /**
     * There are points on the table. Fill AND figure.
     *
     * On a `volt` fill every glyph is [carbon] — 15.09:1. That value
     * inversion is what makes a claim read as signage rather than as
     * neon, and it is why this is not a glow.
     */
    val volt: Color = Color(0xFFDFF23A),
    /** A read that failed, or a figure that is stale. */
    val pulse: Color = Color(0xFFFFA0A8),
    /**
     * A heartbeat is being broadcast. **FILL-ONLY, never type.**
     * 4.91:1 on [slab] and 3.90:1 on [slabHigh] — both clear the 3:1
     * that WCAG 1.4.11 asks of non-text UI state.
     */
    val presence: Color = Color(0xFF4CC15A),
) {
    /** What goes on top of a [volt] fill. Never white. */
    val onVolt: Color get() = carbon

    // --- TRANSITIONAL, DELETE WITH THE SCREEN REWRITE (see bottom) ---
    val ink: Color get() = carbon
    val surface: Color get() = slab
    val surfaceHigh: Color get() = slabHigh
    val outline: Color get() = rule
    val textPrimary: Color get() = focus
    val textSecondary: Color get() = dim
    val textMuted: Color get() = muted
    /** #107C10 does not come back. Fills that used it now use volt — and
     *  anything on top of them must be [onVolt], never white. */
    val brand: Color get() = volt
    val brandBright: Color get() = presence
    val waiting: Color get() = volt
    val danger: Color get() = pulse
}

val LocalXau = staticCompositionLocalOf { XauPalette() }

/**
 * The one spacing scale. 20 is the block gutter.
 *
 * A literal outside this set is a bug — inconsistent density is the
 * loudest signal of a cheap app, and the previous implementation had
 * eleven off-scale values including `Space.touch + 6.dp`, arithmetic
 * used to dodge the scale it claimed to honour.
 */
object Space {
    val xs = 4.dp
    val sm = 8.dp
    val md = 12.dp
    val lg = 16.dp
    /** Block gutter: type insets this far from a full-bleed edge. */
    val gutter = 20.dp
    val xl = 24.dp
    val xxl = 32.dp

    /** Minimum touch target, icon buttons included. */
    val touch = 48.dp
}

/**
 * Full-bleed, square. There is no card in this design.
 *
 * Blocks run edge to edge and separate by value, not by a rounded
 * outline. `seam` is the only curve, and it is nearly imperceptible by
 * design — it softens a tile edge without making it a pill.
 */
object Radius {
    val none = 0.dp
    val seam = 2.dp

    // --- TRANSITIONAL, DELETE WITH THE SCREEN REWRITE ---
    // The old screens still ask for a card radius. They get 0: the
    // rounding goes away immediately even though the layout has not
    // been rebuilt yet.
    val card = none
    val control = seam
    val chip = seam
}

// ---------------------------------------------------------------- type

private val ArchivoExp = FontFamily(
    Font(R.font.archivo_expanded_semibold, FontWeight.W600),
    Font(R.font.archivo_expanded_bold, FontWeight.W700),
)

private val Archivo = FontFamily(
    Font(R.font.archivo_regular, FontWeight.W400),
    Font(R.font.archivo_semibold, FontWeight.W600),
)

private val ArchivoSemiCond = FontFamily(
    Font(R.font.archivo_semicondensed_semibold, FontWeight.W600),
)

private val PlexMono = FontFamily(
    Font(R.font.ibm_plex_mono_regular, FontWeight.W400),
    Font(R.font.ibm_plex_mono_medium, FontWeight.W500),
    Font(R.font.ibm_plex_mono_semibold, FontWeight.W600),
    Font(R.font.ibm_plex_mono_bold, FontWeight.W700),
)

private val Trim = LineHeightStyle(
    alignment = LineHeightStyle.Alignment.Center,
    trim = LineHeightStyle.Trim.None,
)

private fun t(
    family: FontFamily,
    size: Int,
    weight: Int,
    tracking: Float,
    lineHeight: Float,
) = TextStyle(
    fontFamily = family,
    fontSize = size.sp,
    fontWeight = FontWeight(weight),
    // Tracking in em so it scales with the user's font size instead of
    // drifting when they raise it.
    letterSpacing = tracking.em,
    lineHeight = (size * lineHeight).sp,
    lineHeightStyle = Trim,
)

/**
 * The type scale.
 *
 * **The split rule, so this cannot drift:** if the *system* produced it,
 * it is Plex Mono; if it would have been *printed on the panel*, it is
 * Archivo. Points, balances, gamerscores, XUIDs, title IDs, clocks,
 * cycle numbers, ping counts, state words and block headers are mono.
 * Gamertags, sentences, dialogs, tile names and button words are
 * Archivo. Never both inside one cell, and **Plex Mono never carries a
 * sentence** — the moment it does, this becomes hacker cosplay.
 *
 * **Tracking is never negative.** The previous scale used −2% on large
 * text on the theory that it "makes a big number look designed". That
 * is the fintech-hero move that produced the screen this replaces.
 */
@Immutable
data class XauType(
    val verdict: TextStyle = t(ArchivoExp, 28, 700, 0.005f, 1.12f),
    val bandFigure: TextStyle = t(PlexMono, 40, 700, 0f, 1.00f),
    val bandAction: TextStyle = t(ArchivoExp, 20, 700, 0.01f, 1.10f),
    val rowTag: TextStyle = t(Archivo, 16, 600, 0f, 1.25f),
    val rowFigure: TextStyle = t(PlexMono, 18, 600, 0f, 1.20f),
    val rowDelta: TextStyle = t(PlexMono, 13, 600, 0f, 1.20f),
    val blockAction: TextStyle = t(Archivo, 15, 600, 0f, 1.30f),
    val body: TextStyle = t(Archivo, 15, 400, 0f, 1.45f),
    val stateWord: TextStyle = t(PlexMono, 13, 600, 0.04f, 1.20f),
    val tileName: TextStyle = t(ArchivoSemiCond, 12, 600, 0f, 1.15f),
    val register: TextStyle = t(PlexMono, 11, 500, 0.02f, 1.45f),
    val header: TextStyle = t(PlexMono, 10, 600, 0.10f, 1.40f),
    val tileId: TextStyle = t(PlexMono, 9, 400, 0.02f, 1.10f),
) {
    // --- TRANSITIONAL, DELETE WITH THE SCREEN REWRITE (see bottom) ---
    val hero: TextStyle get() = bandFigure
    val figure: TextStyle get() = rowFigure
    val section: TextStyle get() = rowTag
    val rowTitle: TextStyle get() = rowTag
    val secondary: TextStyle get() = body
    val caption: TextStyle get() = register
    val label: TextStyle get() = header
}

val LocalXauType = staticCompositionLocalOf { XauType() }

// --------------------------------------------------------------- theme

@Composable
fun XauTheme(content: @Composable () -> Unit) {
    val palette = XauPalette()
    val type = XauType()

    // Dark only. This app is used at night, next to a console, and a
    // light mode nobody asked for is a second design to keep correct
    // rather than a feature. The system preference is not consulted.
    val scheme = darkColorScheme(
        primary = palette.volt,
        onPrimary = palette.onVolt,
        background = palette.carbon,
        onBackground = palette.focus,
        surface = palette.slab,
        onSurface = palette.focus,
        surfaceVariant = palette.slabHigh,
        onSurfaceVariant = palette.dim,
        outline = palette.rule,
        outlineVariant = palette.rule,
        error = palette.pulse,
        onError = palette.carbon,
    )

    CompositionLocalProvider(LocalXau provides palette, LocalXauType provides type) {
        MaterialTheme(
            colorScheme = scheme,
            typography = MaterialTheme.typography.copy(
                titleLarge = type.verdict,
                titleMedium = type.rowTag,
                bodyLarge = type.body,
                bodyMedium = type.body,
                labelSmall = type.header,
            ),
            content = content,
        )
    }
}

/** Shorthand so screens read `Xau.colors.volt` rather than the local. */
object Xau {
    val colors: XauPalette
        @Composable get() = LocalXau.current
    val type: XauType
        @Composable get() = LocalXauType.current
}

// ------------------------------------------------- migration scaffold
//
// TRANSITIONAL. The old token names live as aliases INSIDE XauPalette,
// XauType and Radius above, so the existing screens still compile.
//
// The Guide spec replaces HomeScreen, Components, LoginScreen and the
// Notice composable outright: full-bleed blocks, 0dp corners, no card
// anywhere. That rewrite is the next piece of work.
//
// What has landed is the PALETTE and the TYPE, poured into the old
// layout -- and the old layout is what the audit condemned. A green
// build here is not the design being done.
//
// Every alias is dead the moment its last caller is rewritten. If they
// still exist once the screens are migrated, that is rot: delete them
// and let the compiler find what is left behind.
