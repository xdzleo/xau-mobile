package com.xau.rewards.ui

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.sizeIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.scale
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import java.text.NumberFormat
import java.util.Locale

/**
 * Shared building blocks.
 *
 * Each one exists to make a `DESIGN.md` rule the default rather than
 * something to remember: [XauCard] cannot cast a shadow, [Figure]
 * cannot print a number it was never given, and [PrimaryButton] is the
 * only thing in the app that fills with brand green.
 */

private val ptBr: NumberFormat = NumberFormat.getInstance(Locale("pt", "BR"))

/** Locale-formatted, or an em dash when the value was never fetched. */
fun money(v: Long?): String = v?.let { ptBr.format(it) } ?: "—"

// ---------------------------------------------------------------- card

// ------------------------------------------------------------- gesture

/**
 * Tap handling with the press state everything clickable owes the user.
 *
 * 0.97 is deliberate: enough that a thumb resting on a card registers,
 * small enough that a list does not appear to breathe.
 */
@Composable
fun Modifier.pressable(onClick: () -> Unit): Modifier {
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by androidx.compose.animation.core.animateFloatAsState(
        targetValue = if (pressed) 0.97f else 1f,
        animationSpec = androidx.compose.animation.core.spring(
            dampingRatio = 0.55f,
            stiffness = 900f,
        ),
        label = "press",
    )
    return this
        .scale(scale)
        .clickable(
            interactionSource = interaction,
            indication = androidx.compose.material3.ripple(color = Xau.colors.textSecondary),
            onClick = onClick,
        )
}

/**
 * [pressable] with a long-press as well.
 *
 * Used where a row has a destructive secondary action that does not
 * deserve permanent screen space — the long press opens a confirmation,
 * so nothing is lost to a mis-grab.
 */
@OptIn(androidx.compose.foundation.ExperimentalFoundationApi::class)
@Composable
fun Modifier.pressable(onClick: () -> Unit, onLongClick: () -> Unit): Modifier {
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by androidx.compose.animation.core.animateFloatAsState(
        targetValue = if (pressed) 0.97f else 1f,
        animationSpec = androidx.compose.animation.core.spring(
            dampingRatio = 0.55f,
            stiffness = 900f,
        ),
        label = "press",
    )
    return this
        .scale(scale)
        .combinedClickable(
            interactionSource = interaction,
            indication = androidx.compose.material3.ripple(color = Xau.colors.textSecondary),
            onClick = onClick,
            onLongClick = onLongClick,
        )
}

// -------------------------------------------------------------- number

/**
 * A figure that animates to its new value.
 *
 * The count-up is what makes a claim feel like being *paid* rather than
 * like a field refreshing. It runs only when a real number replaces
 * another real number — arriving from "never fetched" snaps, because
 * animating up from nothing would imply the account just earned it all.
 */
@Composable
fun Figure(
    value: Long?,
    modifier: Modifier = Modifier,
    style: androidx.compose.ui.text.TextStyle = Xau.type.figure,
    color: Color = Xau.colors.textPrimary,
) {
    if (value == null) {
        Text("—", modifier = modifier, style = style, color = Xau.colors.textMuted)
        return
    }
    val animated by androidx.compose.animation.core.animateFloatAsState(
        targetValue = value.toFloat(),
        animationSpec = tween(
            durationMillis = 600,
            // easeOutCubic: fast to most of the value, then settles.
            easing = androidx.compose.animation.core.CubicBezierEasing(0.22f, 1f, 0.36f, 1f),
        ),
        label = "figure",
    )
    Text(money(animated.toLong()), modifier = modifier, style = style, color = color)
}

// ------------------------------------------------------------ skeleton

/**
 * A loading placeholder shaped like the content it stands in for.
 *
 * `DESIGN.md` §7 bans the centred spinner: a spinner says "something is
 * happening somewhere", a skeleton says "a number is arriving *here*",
 * and the second is the one that stops the layout jumping when it does.
 */
@Composable
fun Skeleton(width: Dp, height: Dp, modifier: Modifier = Modifier) {
    val c = Xau.colors
    val shimmer = rememberInfiniteTransition(label = "shimmer")
    val x by shimmer.animateFloat(
        initialValue = -220f,
        targetValue = 220f,
        animationSpec = infiniteRepeatable(
            animation = tween(1400, easing = LinearEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "x",
    )
    Box(
        modifier
            .size(width, height)
            .clip(RoundedCornerShape(Radius.chip))
            .background(c.surfaceHigh)
            .background(
                Brush.linearGradient(
                    colors = listOf(Color.Transparent, c.outline, Color.Transparent),
                    start = androidx.compose.ui.geometry.Offset(x, 0f),
                    end = androidx.compose.ui.geometry.Offset(x + 160f, 0f),
                )
            )
    )
}

// ------------------------------------------------------------- signals

/** The "this is live" dot. Green as a *signal*, never as a fill. */
@Composable
fun LiveDot(active: Boolean, modifier: Modifier = Modifier) {
    val c = Xau.colors
    val pulse = rememberInfiniteTransition(label = "pulse")
    val alpha by pulse.animateFloat(
        initialValue = 1f,
        targetValue = 0.35f,
        animationSpec = infiniteRepeatable(
            animation = tween(1100, easing = LinearEasing),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "alpha",
    )
    Box(
        modifier
            .size(7.dp)
            .clip(RoundedCornerShape(50))
            .background(
                if (active) c.brandBright.copy(alpha = alpha) else c.textMuted.copy(alpha = 0.5f)
            )
    )
}

/** An uppercase section label. The only place tracking goes positive. */
@Composable
fun SectionLabel(text: String, modifier: Modifier = Modifier) {
    Text(
        text.uppercase(Locale("pt", "BR")),
        modifier = modifier,
        style = Xau.type.label,
        color = Xau.colors.textMuted,
    )
}

// -------------------------------------------------------------- button

/**
 * The primary action. Brand green fill, white label.
 *
 * White on `#107C10` measures 5.36:1 — the only pairing on that fill
 * that clears AA. There is at most one of these on a screen.
 */
@Composable
fun PrimaryButton(
    label: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    busy: Boolean = false,
) {
    val c = Xau.colors
    val bg = if (enabled && !busy) c.brand else c.surfaceHigh
    val fg = if (enabled && !busy) Color.White else c.textMuted

    Box(
        modifier
            .fillMaxWidth()
            .height(Space.touch + 6.dp)
            .clip(RoundedCornerShape(Radius.control))
            .background(bg)
            .then(if (enabled && !busy) Modifier.pressable(onClick) else Modifier),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            if (busy) "Resgatando…" else label,
            style = Xau.type.rowTitle,
            color = fg,
            textAlign = TextAlign.Center,
        )
    }
}

/** A quieter action: outline, no fill, so it never competes for the eye. */
@Composable
fun GhostButton(
    label: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    tint: Color? = null,
) {
    val c = Xau.colors
    Box(
        modifier
            .sizeIn(minHeight = Space.touch)
            .clip(RoundedCornerShape(Radius.control))
            .border(1.dp, c.outline, RoundedCornerShape(Radius.control))
            .pressable(onClick)
            .padding(horizontal = Space.lg, vertical = Space.md),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, style = Xau.type.rowTitle, color = tint ?: c.textSecondary)
    }
}

// -------------------------------------------------------------- states

/**
 * The screen has nothing to show, and says why.
 *
 * `DESIGN.md` §7: an empty list with no explanation is a bug the user
 * cannot report. Every caller must supply what goes here and the action
 * that fills it.
 */
@Composable
fun StateBlock(
    title: String,
    detail: String,
    actionLabel: String? = null,
    onAction: (() -> Unit)? = null,
    tone: Color? = null,
) {
    Column(
        Modifier.fillMaxWidth().padding(Space.xl),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(Space.sm),
    ) {
        Text(title, style = Xau.type.section, color = tone ?: Xau.colors.textPrimary)
        Text(
            detail,
            style = Xau.type.secondary,
            color = Xau.colors.textSecondary,
            textAlign = TextAlign.Center,
        )
        if (actionLabel != null && onAction != null) {
            Box(Modifier.padding(top = Space.sm)) { GhostButton(actionLabel, onAction) }
        }
    }
}

/** A short row of label + value, used wherever a card lists facts. */
@Composable
fun StatRow(label: String, value: String, valueColor: Color? = null) {
    Row(
        Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, style = Xau.type.secondary, color = Xau.colors.textSecondary)
        Text(value, style = Xau.type.rowTitle, color = valueColor ?: Xau.colors.textPrimary)
    }
}
