package com.xau.rewards.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import coil.compose.AsyncImage
import com.xau.rewards.core.Api
import com.xau.rewards.core.LevelUpEntry
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import java.time.Instant
import java.time.LocalDate
import java.time.ZoneId
import java.time.format.DateTimeFormatter

private val gold = Color(0xFFFFC94D)
private val dayFormat = DateTimeFormatter.ofPattern("dd/MM/yyyy")
private val timeFormat = DateTimeFormatter.ofPattern("HH:mm")

/**
 * Log: every Rewards level-up the app has seen, newest first, one banner
 * each, grouped by day. Only accounts that levelled up appear.
 */
@Composable
fun LogScreen(onBack: () -> Unit) {
    val c = Xau.colors
    var entries by remember { mutableStateOf<List<LevelUpEntry>?>(null) }
    LaunchedEffect(Unit) {
        entries = runCatching { Api.levelLog() }.getOrDefault(emptyList())
    }
    BackHandler(onBack = onBack)

    Column(
        Modifier
            .fillMaxSize()
            .background(c.ink)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        Row(
            Modifier.fillMaxWidth().padding(Space.gutter),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                Modifier.size(Space.touch).clip(RoundedCornerShape(Radius.pill)).clickable(onClick = onBack),
                contentAlignment = Alignment.Center,
            ) { Text("‹", style = Xau.type.hero, color = c.textPrimary) }
            Spacer(Modifier.width(Space.sm))
            Column {
                Text("Histórico de níveis", style = Xau.type.section, color = c.textPrimary)
                entries?.let {
                    Text(
                        if (it.isEmpty()) "Nenhuma subida ainda" else "${it.size} subida${if (it.size == 1) "" else "s"} de nível",
                        style = Xau.type.caption,
                        color = c.textSecondary,
                    )
                }
            }
        }

        val list = entries
        when {
            list == null -> Column(Modifier.padding(Space.gutter), verticalArrangement = Arrangement.spacedBy(Space.md)) {
                repeat(3) { Skeleton(320.dp, 84.dp) }
            }

            list.isEmpty() -> Column(
                Modifier.fillMaxWidth().padding(Space.xl),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text("🏆", fontSize = 40.sp)
                Spacer(Modifier.height(Space.md))
                Text("Nenhuma conta subiu de nível ainda", style = Xau.type.rowTitle, color = c.textPrimary)
                Spacer(Modifier.height(Space.xs))
                Text(
                    "Quando uma conta subir de nível no Rewards, ela aparece aqui com o horário.",
                    style = Xau.type.body,
                    color = c.textSecondary,
                )
            }

            else -> LazyColumn(
                contentPadding = PaddingValues(start = Space.gutter, end = Space.gutter, bottom = Space.xxl),
                verticalArrangement = Arrangement.spacedBy(Space.md),
            ) {
                // Newest first, a day header wherever the day changes.
                itemsIndexed(list, key = { _, e -> "${e.xuid}-${e.at}-${e.to}" }) { i, e ->
                    val day = e.day()
                    Column {
                        if (i == 0 || list[i - 1].day() != day) {
                            Text(
                                dayLabel(day),
                                style = Xau.type.captionStrong,
                                color = c.textSecondary,
                                modifier = Modifier.padding(top = if (i == 0) 0.dp else Space.md, bottom = Space.sm),
                            )
                        }
                        Box(Modifier.entrance(i % 8)) { LevelUpBanner(e) }
                    }
                }
            }
        }
    }
}

private fun LevelUpEntry.day(): LocalDate = Instant.ofEpochSecond(at).atZone(ZoneId.systemDefault()).toLocalDate()

private fun dayLabel(day: LocalDate): String {
    val today = LocalDate.now()
    return when (day) {
        today -> "HOJE"
        today.minusDays(1) -> "ONTEM"
        else -> day.format(dayFormat)
    }
}

/** One level-up: who, from which level to which, their best balance, and when. */
@Composable
private fun LevelUpBanner(e: LevelUpEntry) {
    val c = Xau.colors
    val shape = RoundedCornerShape(Radius.card)
    Row(
        Modifier
            .fillMaxWidth()
            .clip(shape)
            .background(Brush.horizontalGradient(listOf(gold.copy(alpha = 0.14f), c.surface, c.surface)))
            .border(1.dp, gold.copy(alpha = 0.35f), shape)
            .padding(Space.lg),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier
                .size(52.dp)
                .clip(RoundedCornerShape(Radius.pill))
                .border(2.dp, gold, RoundedCornerShape(Radius.pill))
                .background(c.surfaceHigh),
            contentAlignment = Alignment.Center,
        ) {
            if (e.gamerpic.isNotEmpty()) {
                AsyncImage(
                    model = e.gamerpic,
                    contentDescription = null,
                    contentScale = ContentScale.Crop,
                    modifier = Modifier.fillMaxSize().clip(RoundedCornerShape(Radius.pill)),
                )
            } else {
                Text(e.gamertag.take(1).uppercase(), style = Xau.type.rowTitle, color = c.textPrimary)
            }
        }
        Spacer(Modifier.width(Space.md))
        Column(Modifier.weight(1f)) {
            Text(
                e.gamertag,
                style = Xau.type.rowTitle,
                color = c.textPrimary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(2.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    "Nível ${e.from} → ${e.to}",
                    style = Xau.type.captionStrong,
                    color = c.ink,
                    modifier = Modifier
                        .clip(RoundedCornerShape(Radius.pill))
                        .background(gold)
                        .padding(horizontal = Space.sm, vertical = 1.dp),
                )
                Spacer(Modifier.width(Space.sm))
                Text(
                    Instant.ofEpochSecond(e.at).atZone(ZoneId.systemDefault()).format(timeFormat),
                    style = Xau.type.caption,
                    color = c.textSecondary,
                )
            }
        }
        Column(horizontalAlignment = Alignment.End) {
            Text(money(e.peak), style = Xau.type.rowFigure, color = c.textPrimary)
            Text("maior saldo", style = Xau.type.caption, color = c.textMuted)
        }
    }
}
