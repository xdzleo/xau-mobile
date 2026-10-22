package com.xau.rewards.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import coil.compose.AsyncImage
import com.xau.rewards.R
import com.xau.rewards.core.Mission
import com.xau.rewards.core.MissionStep
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau

/**
 * One Rewards mission, drawn the way the xbox.com Rewards page draws it:
 * a row of day boxes for streaks (done ones filled and checked, today's
 * outlined), the game covers for monthly packs, a progress bar for the
 * rest, and a status line under it all — what was claimed, what unlocks
 * it, or that the week was lost.
 */
@Composable
fun MissionCard(m: Mission) {
    val c = Xau.colors
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.row))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.row))
            .padding(Space.lg),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            if (m.kind == "single") {
                val art = m.images.firstOrNull()
                if (art != null || m.locked) {
                    Thumb(art, m.locked)
                    Spacer(Modifier.width(Space.md))
                } else {
                    StatusDot(m.done)
                    Spacer(Modifier.width(Space.md))
                }
            }
            Column(Modifier.weight(1f)) {
                Text(
                    // The service does not always name an activity. Its own
                    // status label is a truer fallback than a made-up title.
                    m.title.ifEmpty { m.label.ifEmpty { "Atividade" } },
                    style = Xau.type.rowTitle,
                    color = if (m.done) c.textSecondary else c.textPrimary,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
                if (m.description.isNotEmpty()) {
                    Spacer(Modifier.height(Space.xs))
                    Text(
                        m.description,
                        style = Xau.type.caption,
                        color = c.textSecondary,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            if (m.kind == "single" && m.points > 0 && !m.locked) {
                Spacer(Modifier.width(Space.md))
                Text(
                    "+${money(m.points)}",
                    style = Xau.type.captionStrong,
                    color = if (m.done) c.textMuted else c.green,
                )
            }
        }

        val dim = Modifier.alpha(if (m.locked || m.failed) 0.55f else 1f)
        when (m.kind) {
            "streak" -> if (m.steps.isNotEmpty()) {
                Spacer(Modifier.height(Space.md))
                Row(dim.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(Space.xs)) {
                    m.steps.forEachIndexed { i, s ->
                        StepBox(
                            step = s,
                            label = s.label.ifEmpty { "Dia ${i + 1}" },
                            current = i == m.current,
                            modifier = Modifier.weight(if (s.points > 0 || s.note != null) 1.6f else 1f),
                        )
                    }
                }
            }

            "pack" -> {
                Spacer(Modifier.height(Space.md))
                PackRow(m, dim)
            }

            else -> if (m.locked || (!m.done && m.progress in 1..99)) {
                Spacer(Modifier.height(Space.md))
                Bar(m.progress / 100f, if (m.locked) c.textMuted else c.green)
                if (!m.locked) {
                    Spacer(Modifier.height(Space.xs))
                    Text("${m.progress}%", style = Xau.type.caption, color = c.green)
                }
            }
        }

        StatusLine(m)
    }
}

@Composable
private fun StatusDot(done: Boolean) {
    val c = Xau.colors
    if (done) {
        Box(
            Modifier.size(18.dp).clip(RoundedCornerShape(Radius.pill)).background(c.green),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painterResource(R.drawable.ic_check),
                contentDescription = null,
                tint = c.ink,
                modifier = Modifier.size(12.dp),
            )
        }
    } else {
        Box(
            Modifier
                .size(18.dp)
                .clip(RoundedCornerShape(Radius.pill))
                .border(1.5.dp, c.textMuted, RoundedCornerShape(Radius.pill)),
        )
    }
}

/** The mission's art, with a lock over it when it is out of reach. */
@Composable
private fun Thumb(url: String?, locked: Boolean) {
    val c = Xau.colors
    Box(
        Modifier.size(56.dp).clip(RoundedCornerShape(Radius.control)).background(c.surfaceHigh),
        contentAlignment = Alignment.Center,
    ) {
        if (url != null) {
            AsyncImage(
                model = url,
                contentDescription = null,
                contentScale = ContentScale.Crop,
                modifier = Modifier.fillMaxSize().alpha(if (locked) 0.45f else 1f),
            )
        }
        if (locked) {
            Icon(
                painterResource(R.drawable.ic_lock),
                contentDescription = "Bloqueada",
                tint = c.textPrimary,
                modifier = Modifier.size(22.dp),
            )
        }
    }
}

/** One day of a streak: done ones filled and checked, today's outlined. */
@Composable
private fun StepBox(step: MissionStep, label: String, current: Boolean, modifier: Modifier) {
    val c = Xau.colors
    val shape = RoundedCornerShape(Radius.control)
    Column(
        modifier
            .heightIn(min = 72.dp)
            .clip(shape)
            .background(if (step.done) c.greenDeep.copy(alpha = 0.45f) else c.surfaceHigh)
            .border(
                if (current) 1.5.dp else 1.dp,
                if (current) c.green else c.outline,
                shape,
            )
            .padding(horizontal = Space.sm, vertical = Space.sm),
    ) {
        Text(label, style = Xau.type.caption, color = c.textPrimary, maxLines = 1)
        if (step.points > 0) {
            Text("+${money(step.points)}", style = Xau.type.captionStrong, color = c.textPrimary, maxLines = 1)
        }
        if (step.note != null) {
            Text(step.note, style = Xau.type.caption, color = c.textSecondary, maxLines = 2)
        }
        Spacer(Modifier.weight(1f, fill = false).height(Space.xs))
        StatusDot(step.done)
    }
}

/**
 * The games a monthly pack counts, then numbered slots for whatever the
 * payload had no cover for, so the row always shows the full target.
 * The slot after the last played game is outlined, like the official page.
 */
@Composable
private fun PackRow(m: Mission, modifier: Modifier) {
    val c = Xau.colors
    val target = m.target ?: m.images.size
    val played = m.count ?: 0
    Row(
        modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
        horizontalArrangement = Arrangement.spacedBy(Space.sm),
    ) {
        for (i in 0 until maxOf(target, m.images.size)) {
            val shape = RoundedCornerShape(Radius.control)
            val outline = when {
                i == played && m.current != null -> c.green
                else -> c.outline
            }
            Box(
                Modifier
                    .size(56.dp)
                    .clip(shape)
                    .background(c.surfaceHigh)
                    .border(if (outline == c.green) 1.5.dp else 1.dp, outline, shape),
                contentAlignment = Alignment.Center,
            ) {
                val art = m.images.getOrNull(i)
                if (art != null) {
                    AsyncImage(
                        model = art,
                        contentDescription = null,
                        contentScale = ContentScale.Crop,
                        modifier = Modifier.fillMaxSize(),
                    )
                } else {
                    Text("${i + 1}", style = Xau.type.captionStrong, color = c.textSecondary)
                }
                if (i < played) {
                    Box(Modifier.align(Alignment.BottomEnd).padding(Space.xs)) { StatusDot(true) }
                }
            }
        }
    }
    if (m.target != null) {
        Spacer(Modifier.height(Space.xs))
        Text(
            "${played.coerceAtMost(m.target)}/${m.target} jogos",
            style = Xau.type.caption,
            color = if (played > 0) c.green else c.textSecondary,
        )
    }
}

@Composable
private fun Bar(fraction: Float, tint: Color) {
    val c = Xau.colors
    Box(
        Modifier
            .fillMaxWidth()
            .height(4.dp)
            .clip(RoundedCornerShape(Radius.pill))
            .background(c.surfaceHigh),
    ) {
        if (fraction > 0f) {
            Box(
                Modifier
                    .fillMaxWidth(fraction.coerceIn(0f, 1f))
                    .height(4.dp)
                    .clip(RoundedCornerShape(Radius.pill))
                    .background(tint),
            )
        }
    }
}

/**
 * The bottom-right line of the official card: "Requires Game Pass
 * subscription 🔒 +40", "Try again next week ✕ +110", "Claimed ✓ 6", or
 * how far a streak has got.
 */
@Composable
private fun StatusLine(m: Mission) {
    val c = Xau.colors
    val (text, glyph, tint) = when {
        m.locked -> Triple(
            m.requirement ?: if (mentionsGamePass(m)) "Requer assinatura Game Pass" else m.label.ifEmpty { "Bloqueada" },
            R.drawable.ic_lock,
            c.textSecondary,
        )
        m.failed -> Triple("Tente na próxima semana", null, c.danger)
        m.claimed != null && m.claimed > 0 -> Triple("Resgatado", R.drawable.ic_check, c.green)
        m.kind == "streak" && m.target != null -> Triple("${m.count ?: 0}/${m.target} dias", null, c.textSecondary)
        else -> return
    }
    val figure = when {
        m.locked || m.failed -> "+${money(m.points)}"
        m.claimed != null && m.claimed > 0 -> money(m.claimed)
        else -> null
    }
    Spacer(Modifier.height(Space.md))
    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End, verticalAlignment = Alignment.CenterVertically) {
        Text(text, style = Xau.type.caption, color = tint, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
        if (glyph != null || m.failed) {
            Spacer(Modifier.width(Space.xs))
            if (glyph != null) {
                Icon(painterResource(glyph), contentDescription = null, tint = tint, modifier = Modifier.size(14.dp))
            } else {
                Text("✕", style = Xau.type.captionStrong, color = tint)
            }
        }
        if (figure != null) {
            Spacer(Modifier.width(Space.xs))
            Text(figure, style = Xau.type.captionStrong, color = c.textPrimary)
        }
    }
}

private fun mentionsGamePass(m: Mission): Boolean =
    listOf(m.title, m.id, m.description).any {
        it.contains("game pass", ignoreCase = true) || it.contains("gamepass", ignoreCase = true)
    }
