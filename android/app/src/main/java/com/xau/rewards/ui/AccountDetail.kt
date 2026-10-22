package com.xau.rewards.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
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
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
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
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import coil.compose.AsyncImage
import com.xau.rewards.R
import com.xau.rewards.core.Account
import com.xau.rewards.core.Api
import com.xau.rewards.core.Mission
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau

/**
 * One account, and what it still has to do.
 *
 * The list comes from the service's own activity states rather than from
 * anything this app infers: `progress` is a 0-100 figure Rewards
 * publishes per activity, and `Claimed`/`Available`/`Locked` are its own
 * words. That matters because the home screen's farm bar is an estimate
 * from our cycle clock, and this screen is the ground truth to check it
 * against.
 */
@Composable
fun AccountDetail(account: Account, onBack: () -> Unit) {
    val c = Xau.colors
    var missions by remember(account.xuid) { mutableStateOf<List<Mission>?>(null) }
    var error by remember(account.xuid) { mutableStateOf<String?>(null) }

    LaunchedEffect(account.xuid) {
        runCatching { Api.missions(account.xuid) }
            .onSuccess { missions = it }
            .onFailure { error = it.message }
    }

    val open = missions.orEmpty().filter { it.open }
    val done = missions.orEmpty().filter { it.done }
    // Neither open nor done: behind a requirement. The official page
    // still lists these, with what unlocks them.
    val locked = missions.orEmpty().filter { !it.open && !it.done }

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
                Modifier
                    .size(Space.touch)
                    .clip(RoundedCornerShape(Radius.pill))
                    .clickable(onClick = onBack),
                contentAlignment = Alignment.Center,
            ) {
                Text("‹", style = Xau.type.hero, color = c.textPrimary)
            }
            Spacer(Modifier.width(Space.sm))
            Box(
                Modifier.size(44.dp).clip(RoundedCornerShape(Radius.pill)).background(c.surfaceHigh),
                contentAlignment = Alignment.Center,
            ) {
                if (account.gamerpic.isNotEmpty()) {
                    AsyncImage(
                        model = account.gamerpic,
                        contentDescription = null,
                        modifier = Modifier.fillMaxSize().clip(RoundedCornerShape(Radius.pill)),
                    )
                } else {
                    Text(
                        account.gamertag.take(1).uppercase(),
                        style = Xau.type.rowTitle,
                        color = c.textSecondary,
                    )
                }
            }
            Spacer(Modifier.width(Space.md))
            Column(Modifier.weight(1f)) {
                Text(
                    account.gamertag,
                    style = Xau.type.section,
                    color = c.textPrimary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    account.email ?: "Nível ${account.level ?: "—"} · ${money(account.balance)} pontos",
                    style = Xau.type.caption,
                    color = c.textSecondary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = account.email?.let { Modifier.copyOnTap(it) } ?: Modifier,
                )
            }
        }

        when {
            error != null -> StateBlock(
                title = "Não foi possível ler as missões",
                detail = error.orEmpty(),
                tone = c.danger,
            )

            missions == null -> Column(
                Modifier.fillMaxWidth().padding(Space.gutter),
                verticalArrangement = Arrangement.spacedBy(Space.md),
            ) {
                repeat(4) { DotLeader(260.dp) }
            }

            missions.orEmpty().isEmpty() -> StateBlock(
                title = "Nenhuma missão",
                detail = "O Rewards não listou nenhuma atividade para esta conta.",
            )

            else -> LazyColumn(
                Modifier.fillMaxSize(),
                contentPadding = androidx.compose.foundation.layout.PaddingValues(
                    start = Space.gutter,
                    end = Space.gutter,
                    bottom = Space.xxl,
                ),
                verticalArrangement = Arrangement.spacedBy(Space.sm),
            ) {
                if (open.isNotEmpty()) {
                    item { GroupHead("Faltam", open.size, c.green) }
                    items(open.size) { MissionCard(open[it]) }
                }
                if (locked.isNotEmpty()) {
                    item {
                        Spacer(Modifier.height(Space.md))
                        GroupHead("Bloqueadas", locked.size, c.textMuted)
                    }
                    items(locked.size) { MissionCard(locked[it]) }
                }
                if (done.isNotEmpty()) {
                    item {
                        Spacer(Modifier.height(Space.md))
                        GroupHead("Concluídas", done.size, c.textMuted)
                    }
                    items(done.size) { MissionCard(done[it]) }
                }
            }
        }
    }
}

@Composable
private fun GroupHead(label: String, count: Int, tint: androidx.compose.ui.graphics.Color) {
    Row(
        Modifier.fillMaxWidth().padding(vertical = Space.sm),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("$count", style = Xau.type.rowTitle, color = tint)
        Spacer(Modifier.width(Space.sm))
        Text(label, style = Xau.type.section, color = Xau.colors.textPrimary)
    }
}

