package com.xau.rewards.ui

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay

/**
 * Opening motion: the block rises into place and fades in, `order` steps
 * after the first, on a soft spring.
 *
 * The progress is read inside `graphicsLayer`, so each frame only redraws
 * the layer — nothing recomposes or re-measures while it plays. That is
 * what keeps it smooth at the panel's full refresh rate (120 Hz) instead
 * of dropping frames to layout work.
 */
@Composable
fun Modifier.entrance(order: Int, rise: Dp = 40.dp): Modifier {
    val progress = remember { Animatable(0f) }
    LaunchedEffect(Unit) {
        delay(STAGGER_MS * order)
        progress.animateTo(
            1f,
            spring(dampingRatio = 0.82f, stiffness = Spring.StiffnessLow),
        )
    }
    return this.graphicsLayer {
        val p = progress.value
        alpha = p.coerceIn(0f, 1f)
        translationY = (1f - p) * rise.toPx()
        val s = 0.95f + 0.05f * p
        scaleX = s
        scaleY = s
    }
}

/**
 * A figure that counts up to its value when it first appears and glides
 * to each new value after that. Null stays null: an unknown figure is
 * never animated into looking like a real one.
 */
@Composable
fun countUp(target: Long?): Long? {
    val value = remember { Animatable(0f) }
    LaunchedEffect(target) {
        if (target != null) {
            value.animateTo(target.toFloat(), tween(durationMillis = 900, easing = FastOutSlowInEasing))
        }
    }
    return target?.let { value.value.toLong() }
}

private const val STAGGER_MS = 60L
