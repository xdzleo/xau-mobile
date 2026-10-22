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
                val art = m.thumbnail
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
                            label = ptBr(s.label).ifEmpty { "Dia ${i + 1}" },
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
            Text(ptBr(step.note), style = Xau.type.caption, color = c.textSecondary, maxLines = 2)
        }
        Spacer(Modifier.weight(1f, fill = false).height(Space.xs))
        StatusDot(step.done)
    }
}

/**
 * A monthly pack's game slots: the cover of each game already played,
 * the slot number for the rest, and the next slot outlined — the same
 * row the official page shows.
 */
@Composable
private fun PackRow(m: Mission, modifier: Modifier) {
    val c = Xau.colors
    Row(
        modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
        horizontalArrangement = Arrangement.spacedBy(Space.sm),
    ) {
        m.steps.forEachIndexed { i, s ->
            val shape = RoundedCornerShape(Radius.control)
            val next = i == m.current
            Box(
                Modifier
                    .size(64.dp)
                    .clip(shape)
                    .background(c.surfaceHigh)
                    .border(if (next) 1.5.dp else 1.dp, if (next) c.green else c.outline, shape),
                contentAlignment = Alignment.Center,
            ) {
                if (s.image != null) {
                    AsyncImage(
                        model = s.image,
                        contentDescription = null,
                        contentScale = ContentScale.Crop,
                        modifier = Modifier.fillMaxSize(),
                    )
                } else if (s.done) {
                    // Played, but titlehub had no picture for it.
                    StatusDot(true)
                } else {
                    Text("${i + 1}", style = Xau.type.captionStrong, color = c.textPrimary)
                }
            }
        }
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
 * The footer of the official card. Left, the time the period has left
 * ("⏱ 9d left"); right, the outcome: "Requires Game Pass subscription
 * 🔒 +40", "Try again next week ✕ +110" or "Claimed ✓ 80".
 */
@Composable
private fun StatusLine(m: Mission) {
    val c = Xau.colors
    val outcome: Triple<String, Int?, Color>? = when {
        m.locked -> Triple(
            m.requirement ?: if (mentionsGamePass(m)) "Requer assinatura Game Pass" else m.label.ifEmpty { "Bloqueada" },
            R.drawable.ic_lock,
            c.textSecondary,
        )
        m.failed -> Triple("Tente na próxima semana", null, c.danger)
        m.claimed != null && m.claimed > 0 -> Triple("Resgatado", R.drawable.ic_check, c.green)
        m.kind != "single" && m.target != null -> Triple(
            "${m.count ?: 0}/${m.target} ${if (m.kind == "pack") "jogos" else "dias"}",
            null,
            c.textSecondary,
        )
        else -> null
    }
    // A finished or lost period has nothing left to count down.
    val left = if (m.locked || m.failed || m.done) null else daysLeft(m.resetDate)
    if (outcome == null && left == null) return

    val figure = when {
        m.locked || m.failed -> "+${money(m.points)}"
        m.claimed != null && m.claimed > 0 -> money(m.claimed)
        else -> null
    }
    Spacer(Modifier.height(Space.md))
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        if (left != null) {
            Icon(
                painterResource(R.drawable.ic_clock),
                contentDescription = null,
                tint = c.textSecondary,
                modifier = Modifier.size(14.dp),
            )
            Spacer(Modifier.width(Space.xs))
            Text(left, style = Xau.type.caption, color = c.textSecondary, maxLines = 1)
        }
        Spacer(Modifier.weight(1f))
        if (outcome != null) {
            val (text, glyph, tint) = outcome
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
}

/** "9d restantes" until an ISO-8601 reset, rounded up like the page. */
private fun daysLeft(iso: String?): String? {
    val end = iso?.let { runCatching { java.time.Instant.parse(it) }.getOrNull() } ?: return null
    val secs = java.time.Duration.between(java.time.Instant.now(), end).seconds
    if (secs <= 0) return null
    val hours = (secs + 3599) / 3600
    return if (hours < 24) "${hours}h restantes" else "${(hours + 23) / 24}d restantes"
}

/** The service answers in English; the app speaks Portuguese. */
private fun ptBr(s: String): String =
    s.replace(Regex("""^Day (\d+)$"""), "Dia $1")
        .replace("Next week", "Próx. semana")

private fun mentionsGamePass(m: Mission): Boolean =
    listOf(m.title, m.id, m.description).any {
        it.contains("game pass", ignoreCase = true) || it.contains("gamepass", ignoreCase = true)
    }
