package com.xau.rewards.ui

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
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
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
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
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import coil.compose.AsyncImage
import com.xau.rewards.R
import com.xau.rewards.UiState
import com.xau.rewards.core.Account
import com.xau.rewards.core.FarmTitle
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import kotlinx.coroutines.delay

/**
 * The home screen: the day at a glance, the farm's games, the accounts.
 *
 * Three blocks, in the order a user asks the questions: how much have I
 * got (the hero), what is the farm playing (the games), and what is each
 * account doing (the cards). The farm switch lives on the account card,
 * because that is the thing there is more than one of and the thing the
 * core keys farms by.
 */
@Composable
fun HomeScreen(
    state: UiState,
    onClaim: (String) -> Unit,
    onSelect: (String) -> Unit,
    onSignOut: (String) -> Unit,
    onAddAccount: () -> Unit,
    onToggleFarm: (String) -> Unit,
    onOpenAccount: (Account) -> Unit = {},
    onOpenOffers: () -> Unit = {},
    onRetry: () -> Unit = {},
) {
    val c = Xau.colors
    var pendingRemoval by remember { mutableStateOf<Account?>(null) }

    pendingRemoval?.let { target ->
        RemovalDialog(target, onConfirm = { onSignOut(target.xuid); pendingRemoval = null }) {
            pendingRemoval = null
        }
    }

    Box(
        Modifier
            .fillMaxSize()
            .background(c.ink),
    ) {
        // The glow behind the header: the one place the brand colour is
        // allowed to be atmosphere rather than signal.
        Box(
            Modifier
                .fillMaxWidth()
                .height(360.dp)
                .background(
                    Brush.radialGradient(
                        colors = listOf(c.greenDeep.copy(alpha = 0.45f), Color.Transparent),
                        center = Offset(180f, -40f),
                        radius = 900f,
                    ),
                ),
        )

        Column(Modifier.fillMaxSize().windowInsetsPadding(WindowInsets.safeDrawing)) {
            LazyColumn(
                Modifier.weight(1f).fillMaxWidth(),
                contentPadding = PaddingValues(
                    start = Space.gutter,
                    end = Space.gutter,
                    top = Space.md,
                    bottom = Space.xxl,
                ),
                verticalArrangement = Arrangement.spacedBy(Space.lg),
            ) {
                item { Box(Modifier.entrance(0)) { TopBar(onRetry) } }
                item { Box(Modifier.entrance(1)) { HeroCard(state) } }
                if (state.farmTitles.isNotEmpty()) {
                    item {
                        Box(Modifier.entrance(2)) {
                            FarmGamesCard(state.farmTitles, farming = state.accounts.any { it.farming })
                        }
                    }
                }
                item { Box(Modifier.entrance(3)) { AccountsHead(state.accounts.size, onAddAccount) } }

                if (state.booting) {
                    items(2) { AccountCardSkeleton() }
                } else {
                    items(state.accounts.size, key = { state.accounts[it].xuid }) { i ->
                        val a = state.accounts[i]
                        Box(Modifier.entrance(4 + i)) {
                            AccountCard(
                                account = a,
                                titles = state.farmTitles,
                                claiming = state.claimingXuid == a.xuid,
                                onSelect = { onSelect(a.xuid); onOpenAccount(a) },
                                onLongPress = { pendingRemoval = a },
                                onToggleFarm = { onToggleFarm(a.xuid) },
                                onClaim = { onClaim(a.xuid) },
                            )
                        }
                    }
                }
            }

            Box(Modifier.entrance(2, rise = 90.dp)) {
                NavBar(onAdd = onAddAccount, onOpenOffers = onOpenOffers)
            }
        }
    }
}

// ------------------------------------------------------------- top bar

@Composable
private fun TopBar(onRefresh: () -> Unit) {
    val c = Xau.colors
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Box(
            Modifier.size(40.dp).clip(RoundedCornerShape(Radius.pill)).background(Color.White),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painterResource(R.drawable.ic_xbox),
                contentDescription = null,
                tint = c.greenDeep,
                modifier = Modifier.size(27.dp),
            )
        }
        Spacer(Modifier.width(Space.md))
        Column(Modifier.weight(1f)) {
            Text(
                buildAnnotatedString {
                    append("Xbox ")
                    withStyle(SpanStyle(color = c.green)) { append("Rewards") }
                },
                style = Xau.type.brand,
                color = c.textPrimary,
            )
            Text("Jogos. Presença. Recompensas.", style = Xau.type.caption, color = c.textSecondary)
        }
        RoundIconButton(R.drawable.ic_refresh, "Atualizar", onRefresh)
    }
}

@Composable
private fun RoundIconButton(icon: Int, description: String, onClick: () -> Unit) {
    val c = Xau.colors
    Box(
        Modifier
            .size(Space.touch)
            .clip(RoundedCornerShape(Radius.pill))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.pill))
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            painterResource(icon),
            contentDescription = description,
            tint = c.textPrimary,
            modifier = Modifier.size(20.dp),
        )
    }
}

// ---------------------------------------------------------------- hero

/**
 * The total, how many accounts are farming, and the three numbers worth
 * acting on. One card, so the answer to "how am I doing" is one glance.
 */
@Composable
private fun HeroCard(state: UiState) {
    val c = Xau.colors
    val total = state.accounts.size
    val farming = state.accounts.count { it.farming }
    val withGp = state.accounts.count { it.gamepass != null }
    val waiting = state.totalWaiting
    val shape = RoundedCornerShape(Radius.card + 4.dp)

    Column(
        Modifier
            .fillMaxWidth()
            .clip(shape)
            .background(
                Brush.linearGradient(
                    colors = listOf(Color(0xFF0E5A10), Color(0xFF0B2A0C), c.surface),
                    start = Offset(0f, 0f),
                    end = Offset(900f, 900f),
                ),
            )
            .border(1.dp, c.green.copy(alpha = 0.35f), shape)
            .padding(Space.xl),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("SALDO TOTAL", style = Xau.type.captionStrong, color = c.green)
                Spacer(Modifier.height(Space.xs))
                Text(
                    if (state.booting) "—" else money(countUp(state.totalBalance)),
                    style = Xau.type.hero,
                    color = c.textPrimary,
                    maxLines = 1,
                )
                Text(
                    when (total) {
                        0 -> "nenhuma conta conectada"
                        1 -> "pontos em 1 conta"
                        else -> "pontos em $total contas"
                    },
                    style = Xau.type.body,
                    color = c.textSecondary,
                )
            }
            Spacer(Modifier.width(Space.md))
            ProgressRing(farming, total)
        }

        Spacer(Modifier.height(Space.lg))

        Row(horizontalArrangement = Arrangement.spacedBy(Space.sm)) {
            StatTile(
                label = "Farmando",
                value = "$farming/$total",
                live = farming > 0,
                modifier = Modifier.weight(1f),
            )
            StatTile(
                label = "Game Pass",
                value = "$withGp/$total",
                modifier = Modifier.weight(1f),
            )
            StatTile(
                label = "A resgatar",
                value = money(waiting),
                accent = waiting > 0,
                modifier = Modifier.weight(1f),
            )
        }
    }
}

@Composable
private fun StatTile(
    label: String,
    value: String,
    modifier: Modifier = Modifier,
    live: Boolean = false,
    accent: Boolean = false,
) {
    val c = Xau.colors
    Column(
        modifier
            .clip(RoundedCornerShape(Radius.control))
            .background(Color.Black.copy(alpha = 0.28f))
            .padding(horizontal = Space.md, vertical = Space.md),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            if (live) {
                LiveDot(active = true)
                Spacer(Modifier.width(Space.xs))
            }
            Text(label, style = Xau.type.caption, color = c.textSecondary, maxLines = 1)
        }
        Spacer(Modifier.height(2.dp))
        Text(
            value,
            style = Xau.type.rowFigure,
            color = if (accent) c.green else c.textPrimary,
            maxLines = 1,
        )
    }
}

@Composable
private fun ProgressRing(done: Int, total: Int) {
    val c = Xau.colors
    val fraction = if (total == 0) 0f else done.toFloat() / total
    val animated by animateFloatAsState(
        targetValue = fraction,
        animationSpec = tween(durationMillis = 700),
        label = "ring",
    )
    Box(Modifier.size(104.dp), contentAlignment = Alignment.Center) {
        Canvas(Modifier.fillMaxSize()) {
            val stroke = 10.dp.toPx()
            val inset = stroke / 2
            val arcSize = Size(size.width - stroke, size.height - stroke)
            drawArc(
                color = Color.Black.copy(alpha = 0.35f),
                startAngle = 0f,
                sweepAngle = 360f,
                useCenter = false,
                topLeft = Offset(inset, inset),
                size = arcSize,
                style = Stroke(width = stroke, cap = StrokeCap.Round),
            )
            if (animated > 0f) {
                drawArc(
                    brush = Brush.sweepGradient(listOf(c.greenDeep, c.green, c.greenDeep)),
                    startAngle = -90f,
                    sweepAngle = 360f * animated,
                    useCenter = false,
                    topLeft = Offset(inset, inset),
                    size = arcSize,
                    style = Stroke(width = stroke, cap = StrokeCap.Round),
                )
            }
        }
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            Text("$done/$total", style = Xau.type.ringFigure, color = c.textPrimary)
            Text("no farm", style = Xau.type.caption, color = c.textSecondary)
        }
    }
}

// --------------------------------------------------------- farm games

/**
 * The eight titles the farm plays, with their real box art. They are not
 * a choice — every farm plays the same eight — so they are shown once,
 * as what the presence looks like, not as a list to pick from.
 */
@Composable
private fun FarmGamesCard(titles: List<FarmTitle>, farming: Boolean) {
    val c = Xau.colors
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.card))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.card))
            .padding(Space.lg),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(
                painterResource(R.drawable.ic_pad),
                contentDescription = null,
                tint = c.green,
                modifier = Modifier.size(20.dp),
            )
            Spacer(Modifier.width(Space.sm))
            Column(Modifier.weight(1f)) {
                Text("Jogos do farm", style = Xau.type.section, color = c.textPrimary)
                Text(
                    "${titles.size} jogos de console · 15 min",
                    style = Xau.type.caption,
                    color = c.textSecondary,
                )
            }
            StatusPill(
                text = if (farming) "Online" else "Parado",
                live = farming,
            )
        }

        Spacer(Modifier.height(Space.lg))

        titles.chunked(4).forEachIndexed { row, chunk ->
            if (row > 0) Spacer(Modifier.height(Space.md))
            Row(horizontalArrangement = Arrangement.spacedBy(Space.sm)) {
                chunk.forEach { t ->
                    Column(Modifier.weight(1f), horizontalAlignment = Alignment.CenterHorizontally) {
                        GameArt(
                            t,
                            Modifier.fillMaxWidth().aspectRatio(1f),
                            corner = Radius.control,
                            dim = !farming,
                        )
                        Spacer(Modifier.height(Space.xs))
                        Text(
                            t.name,
                            style = Xau.type.caption.copy(fontSize = 10.sp),
                            color = c.textSecondary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            textAlign = TextAlign.Center,
                        )
                    }
                }
                // A short last row keeps its tiles the same width.
                repeat(4 - chunk.size) { Spacer(Modifier.weight(1f)) }
            }
        }
    }
}

@Composable
private fun StatusPill(text: String, live: Boolean) {
    val c = Xau.colors
    Row(
        Modifier
            .clip(RoundedCornerShape(Radius.pill))
            .background(if (live) c.green.copy(alpha = 0.14f) else c.surfaceHigh)
            .padding(horizontal = Space.md, vertical = Space.xs + 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        LiveDot(active = live)
        Spacer(Modifier.width(Space.xs + 2.dp))
        Text(text, style = Xau.type.captionStrong, color = if (live) c.green else c.textMuted)
    }
}

/** Box art, or the title's initials on a tile when there is none. */
@Composable
private fun GameArt(title: FarmTitle, modifier: Modifier, corner: Dp, dim: Boolean = false) {
    val c = Xau.colors
    Box(
        modifier
            .clip(RoundedCornerShape(corner))
            .background(c.surfaceHigh),
        contentAlignment = Alignment.Center,
    ) {
        if (title.image != null) {
            AsyncImage(
                model = title.image,
                contentDescription = title.name,
                contentScale = ContentScale.Crop,
                alpha = if (dim) 0.55f else 1f,
                modifier = Modifier.fillMaxSize(),
            )
        } else {
            Text(
                initials(title.name),
                style = Xau.type.captionStrong,
                color = c.textSecondary,
            )
        }
    }
}

private fun initials(name: String): String =
    name.split(' ', ':', '-')
        .filter { it.isNotBlank() && it[0].isLetterOrDigit() }
        .take(2)
        .joinToString("") { it.take(1).uppercase() }

// ------------------------------------------------------------ accounts

@Composable
private fun AccountsHead(count: Int, onAdd: () -> Unit) {
    val c = Xau.colors
    Row(
        Modifier.fillMaxWidth().padding(top = Space.sm),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text("Contas", style = Xau.type.section, color = c.textPrimary)
        Spacer(Modifier.width(Space.sm))
        Box(
            Modifier
                .clip(RoundedCornerShape(Radius.pill))
                .background(c.surfaceHigh)
                .padding(horizontal = Space.sm, vertical = 2.dp),
        ) {
            Text("$count", style = Xau.type.captionStrong, color = c.textSecondary)
        }
        Spacer(Modifier.weight(1f))
        Text(
            "+ Adicionar",
            style = Xau.type.button,
            color = c.green,
            modifier = Modifier
                .clip(RoundedCornerShape(Radius.pill))
                .clickable(onClick = onAdd)
                .padding(horizontal = Space.md, vertical = Space.sm),
        )
    }
}

@Composable
private fun AccountCard(
    account: Account,
    titles: List<FarmTitle>,
    claiming: Boolean,
    onSelect: () -> Unit,
    onLongPress: () -> Unit,
    onToggleFarm: () -> Unit,
    onClaim: () -> Unit,
) {
    val c = Xau.colors
    val haptics = LocalHapticFeedback.current
    val waiting = account.unclaimed ?: 0L
    val stale = account.staleReason != null
    val shape = RoundedCornerShape(Radius.card)

    Column(
        Modifier
            .fillMaxWidth()
            .clip(shape)
            .background(
                if (account.farming) {
                    Brush.verticalGradient(listOf(Color(0xFF13261A), c.surface))
                } else {
                    Brush.verticalGradient(listOf(c.surface, c.surface))
                },
            )
            .border(
                1.dp,
                if (account.farming) c.green.copy(alpha = 0.55f) else c.outline,
                shape,
            )
            .pressableRow(onClick = onSelect, onLongClick = onLongPress)
            .padding(Space.lg),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Avatar(account, farming = account.farming)
            Spacer(Modifier.width(Space.md))
            Column(Modifier.weight(1f)) {
                Text(
                    account.gamertag.ifEmpty { account.xuid },
                    style = Xau.type.rowTitle,
                    color = c.textPrimary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                account.email?.let {
                    Text(
                        it,
                        style = Xau.type.caption,
                        color = c.textMuted,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            // The farm switch lives per account, because the core keys
            // farms by xuid and each account earns on its own.
            Switch(
                checked = account.farming,
                onCheckedChange = {
                    haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                    onToggleFarm()
                },
                colors = SwitchDefaults.colors(
                    checkedThumbColor = Color.White,
                    checkedTrackColor = c.green,
                    checkedBorderColor = c.green,
                    uncheckedThumbColor = c.textMuted,
                    uncheckedTrackColor = c.surfaceHigh,
                    uncheckedBorderColor = c.outline,
                ),
            )
        }

        Spacer(Modifier.height(Space.md))

        // Badges on their own line: crowding them next to the name let a
        // long tier squeeze the gamertag down to "xd…" on a real handset.
        Row(
            horizontalArrangement = Arrangement.spacedBy(Space.sm),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            LevelBadge(account.level, stale)
            SubscriptionBadge(account)
        }

        Spacer(Modifier.height(Space.md))

        Row(verticalAlignment = Alignment.Bottom) {
            Column(Modifier.weight(1f)) {
                Text(
                    money(countUp(account.balance)),
                    style = Xau.type.hero.copy(fontSize = 30.sp),
                    color = if (stale) c.danger else c.textPrimary,
                )
                Text("pontos", style = Xau.type.caption, color = c.textMuted)
            }
            account.missionsOpen?.let { open ->
                Column(horizontalAlignment = Alignment.End) {
                    Text(
                        "$open",
                        style = Xau.type.rowFigure,
                        color = if (open > 0) c.textPrimary else c.textMuted,
                    )
                    Text(
                        if (open == 1) "missão aberta" else "missões abertas",
                        style = Xau.type.caption,
                        color = c.textMuted,
                    )
                }
            }
        }

        // The mission is "play 8 titles for 15 minutes"; the farm
        // broadcasts all eight at once, so one cycle IS the mission and
        // its progress is the mission's progress.
        if (account.farming) {
            Spacer(Modifier.height(Space.lg))
            MissionProgress(account, titles)
            // A farm whose pings Xbox refuses still fills its bar; say so,
            // or the mission looks like it is counting when it is not.
            farmPresenceProblem(account.farmPresence, account.farmPresenceHttp)?.let { why ->
                Spacer(Modifier.height(Space.sm))
                Text(
                    why,
                    style = Xau.type.caption,
                    color = c.danger,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }

        if (stale) {
            Spacer(Modifier.height(Space.sm))
            Text(
                account.staleReason.orEmpty(),
                style = Xau.type.caption,
                color = c.danger,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }

        if (waiting > 0) {
            Spacer(Modifier.height(Space.md))
            Box(
                Modifier
                    .fillMaxWidth()
                    .height(Space.touch)
                    .clip(RoundedCornerShape(Radius.control))
                    .background(c.greenDeep)
                    .clickable(enabled = !claiming) {
                        haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                        onClaim()
                    },
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    if (claiming) "Resgatando…" else "Resgatar ${money(waiting)} pontos",
                    style = Xau.type.button,
                    color = c.onGreen,
                )
            }
        }
    }
}

/** Gamerpic in a ring that turns green while the account is farming. */
@Composable
private fun Avatar(account: Account, farming: Boolean) {
    val c = Xau.colors
    Box(contentAlignment = Alignment.BottomEnd) {
        Box(
            Modifier
                .size(54.dp)
                .clip(RoundedCornerShape(Radius.pill))
                .background(
                    if (farming) {
                        Brush.sweepGradient(listOf(c.green, c.greenDeep, c.green))
                    } else {
                        Brush.linearGradient(listOf(c.outline, c.outline))
                    },
                )
                .padding(2.5.dp)
                .clip(RoundedCornerShape(Radius.pill))
                .background(c.surfaceHigh),
            contentAlignment = Alignment.Center,
        ) {
            if (account.gamerpic.isNotEmpty()) {
                AsyncImage(
                    model = account.gamerpic,
                    contentDescription = null,
                    contentScale = ContentScale.Crop,
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
        if (farming) {
            Box(
                Modifier
                    .size(14.dp)
                    .clip(RoundedCornerShape(Radius.pill))
                    .background(c.ink)
                    .padding(2.5.dp)
                    .clip(RoundedCornerShape(Radius.pill))
                    .background(c.green),
            )
        }
    }
}

/**
 * The Rewards level, 1 to 3.
 *
 * An unknown level renders as "Nível —" and NOT as level 1: an account
 * that has not levelled up and an account whose level could not be read
 * are different situations, and this badge exists precisely to tell them
 * apart.
 */
@Composable
private fun LevelBadge(level: Long?, stale: Boolean) {
    val c = Xau.colors
    val tint = when {
        stale || level == null -> c.textMuted
        level >= 2L -> c.green
        else -> c.textSecondary
    }
    Row(
        Modifier
            .clip(RoundedCornerShape(Radius.pill))
            .background(c.surfaceHigh)
            .padding(horizontal = Space.sm + 2.dp, vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(6.dp).clip(RoundedCornerShape(Radius.pill)).background(tint))
        Spacer(Modifier.width(Space.xs + 1.dp))
        Text("Nível ${level ?: "—"}", style = Xau.type.captionStrong, color = tint)
    }
}

/**
 * The subscription, always stated once it is known.
 *
 * Read from `gamePassMembership` in the same programInfo response the
 * balance comes from, so it costs no extra request. Before that read
 * lands (balance still null) nothing is claimed either way; after it, an
 * absent membership is said out loud as "Sem Game Pass" — which is
 * exactly why such an account sits at level 1 and cannot reach the Game
 * Pass missions.
 */
@Composable
private fun SubscriptionBadge(account: Account) {
    val c = Xau.colors
    val tier = account.gamepass
    when {
        tier != null -> {
            val ultimate = tier.contains("Ultimate", true)
            Row(
                Modifier
                    .clip(RoundedCornerShape(Radius.pill))
                    .background(
                        if (ultimate) {
                            Brush.horizontalGradient(listOf(Color(0xFF9BF00B), c.green))
                        } else {
                            Brush.horizontalGradient(listOf(c.greenDeep, c.greenDeep))
                        },
                    )
                    .padding(horizontal = Space.sm + 2.dp, vertical = 3.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(
                    painterResource(R.drawable.ic_xbox),
                    contentDescription = null,
                    tint = if (ultimate) c.ink else c.onGreen,
                    modifier = Modifier.size(12.dp),
                )
                Spacer(Modifier.width(Space.xs + 1.dp))
                Text(
                    prettyTier(tier),
                    style = Xau.type.captionStrong,
                    color = if (ultimate) c.ink else c.onGreen,
                )
            }
        }
        account.balance != null -> Row(
            Modifier
                .clip(RoundedCornerShape(Radius.pill))
                .border(1.dp, c.outline, RoundedCornerShape(Radius.pill))
                .padding(horizontal = Space.sm + 2.dp, vertical = 2.dp),
        ) {
            Text("Sem Game Pass", style = Xau.type.captionStrong, color = c.textMuted)
        }
        else -> {}
    }
}

/** `GamePassUltimate` -> `Game Pass Ultimate`, `PCGamePass` -> `PC Game Pass`. */
private fun prettyTier(tier: String): String = when {
    tier.contains("Ultimate", true) -> "Game Pass Ultimate"
    tier.contains("PC", true) -> "PC Game Pass"
    tier.contains("Console", true) -> "Game Pass Console"
    tier.contains("Core", true) -> "Game Pass Core"
    tier.contains("Standard", true) -> "Game Pass Standard"
    else -> "Game Pass"
}

@Composable
private fun AccountCardSkeleton() {
    val c = Xau.colors
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.card))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.card))
            .padding(Space.lg),
        verticalArrangement = Arrangement.spacedBy(Space.md),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Box(Modifier.size(54.dp).clip(RoundedCornerShape(Radius.pill)).background(c.surfaceHigh))
            Spacer(Modifier.width(Space.md))
            Column(verticalArrangement = Arrangement.spacedBy(Space.xs)) {
                DotLeader(140.dp)
                DotLeader(90.dp)
            }
        }
        DotLeader(110.dp)
    }
}

// ------------------------------------------------------------- nav bar

/**
 * A floating bar with only what works: home, the add-account action in
 * the middle where the thumb lands, and Vantagens. The inert "Atividade"
 * and "Configurações" tabs are gone — a tab that does nothing is a lie
 * the user finds out by tapping it.
 */
@Composable
private fun NavBar(onAdd: () -> Unit, onOpenOffers: () -> Unit) {
    val c = Xau.colors
    val shape = RoundedCornerShape(Radius.pill)
    Box(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = Space.xxl, vertical = Space.sm),
        contentAlignment = Alignment.Center,
    ) {
        Row(
            Modifier
                .fillMaxWidth()
                .height(68.dp)
                .clip(shape)
                .background(Brush.verticalGradient(listOf(c.surfaceHigh, c.surface)))
                .border(1.dp, c.outline, shape)
                .padding(horizontal = Space.sm),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            NavItem(R.drawable.ic_home, "Início", selected = true, onClick = {}, modifier = Modifier.weight(1f))
            Spacer(Modifier.width(72.dp))
            NavItem(R.drawable.ic_books, "Vantagens", selected = false, onClick = onOpenOffers, modifier = Modifier.weight(1f))
        }
        // The one action that is always available, raised above the bar.
        Box(
            Modifier
                .offset(y = (-6).dp)
                .size(60.dp)
                .clip(RoundedCornerShape(Radius.pill))
                .background(Brush.linearGradient(listOf(c.green, c.greenDeep)))
                .clickable(onClick = onAdd),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painterResource(R.drawable.ic_plus),
                contentDescription = "Adicionar conta",
                tint = Color.White,
                modifier = Modifier.size(28.dp),
            )
        }
    }
}

@Composable
private fun NavItem(
    icon: Int,
    label: String,
    selected: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val c = Xau.colors
    Box(modifier, contentAlignment = Alignment.Center) {
        Row(
            Modifier
                .clip(RoundedCornerShape(Radius.pill))
                .background(if (selected) c.green.copy(alpha = 0.16f) else Color.Transparent)
                .clickable(onClick = onClick)
                .padding(horizontal = Space.lg, vertical = Space.sm + 2.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                painterResource(icon),
                contentDescription = label,
                tint = if (selected) c.green else c.textSecondary,
                modifier = Modifier.size(20.dp),
            )
            Spacer(Modifier.width(Space.sm))
            Text(
                label,
                style = Xau.type.button,
                color = if (selected) c.green else c.textSecondary,
                maxLines = 1,
            )
        }
    }
}

// ------------------------------------------------------------- mission

/**
 * How far this account is through the 8-titles-for-15-minutes mission,
 * with the eight titles it is playing right above the bar.
 *
 * The core only reports progress when the UI polls it, which is once a
 * minute — so drawing the snapshot directly would make the bar jump in
 * minute-sized steps and the clock sit frozen between them. Instead the
 * snapshot is treated as a fix taken at a known instant, and the seconds
 * since are added locally. The bar is then animated to that target, so
 * it advances smoothly and a poll that confirms what was already
 * predicted causes no visible jump at all.
 *
 * When the mission completes the points land as unclaimed and the
 * auto-claim settles them without a tap, so filling this bar is the
 * whole interaction.
 */
@Composable
private fun MissionProgress(account: Account, titles: List<FarmTitle>) {
    val c = Xau.colors
    val total = (account.farmTotal ?: 900L).coerceAtLeast(1L)
    val fix = account.farmElapsed ?: 0L

    // Restarts whenever a fresh snapshot arrives, so local counting is
    // always measured from the most recent real number rather than
    // drifting away from it.
    var since by remember(account.xuid, fix) { mutableStateOf(0L) }
    LaunchedEffect(account.xuid, fix) {
        while (true) {
            delay(1_000)
            since += 1
        }
    }

    val elapsed = (fix + since).coerceIn(0L, total)
    val remaining = (total - elapsed).coerceAtLeast(0L)
    val target = elapsed.toFloat() / total.toFloat()

    val animated by animateFloatAsState(
        targetValue = target,
        // One second of travel for one second of progress: the bar moves
        // at the same rate the clock does, so they never disagree.
        animationSpec = tween(durationMillis = 1_000, easing = LinearEasing),
        label = "mission",
    )

    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.control))
            .background(Color.Black.copy(alpha = 0.25f))
            .padding(Space.md),
    ) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            if (titles.isNotEmpty()) {
                TitleStrip(titles)
                Spacer(Modifier.width(Space.sm))
            }
            Column(Modifier.weight(1f)) {
                Text("Missão de 15 min", style = Xau.type.captionStrong, color = c.textPrimary)
                Text("${titles.size.takeIf { it > 0 } ?: 8} jogos em presença", style = Xau.type.caption, color = c.textMuted)
            }
            Column(horizontalAlignment = Alignment.End) {
                Text(
                    clock(remaining),
                    style = Xau.type.rowFigure,
                    color = if (remaining == 0L) c.green else c.textPrimary,
                )
                Text("${(target * 100).toInt()}%", style = Xau.type.captionStrong, color = c.green)
            }
        }
        Spacer(Modifier.height(Space.sm + 2.dp))
        // Drawn, not laid out: the fill is read in the draw phase, so the
        // bar advancing every frame never re-measures anything.
        val fill = Brush.horizontalGradient(listOf(c.greenDeep, c.green))
        val track = c.surfaceHigh
        Box(
            Modifier
                .fillMaxWidth()
                .height(8.dp)
                .drawBehind {
                    val r = CornerRadius(size.height / 2, size.height / 2)
                    drawRoundRect(color = track, cornerRadius = r)
                    val w = size.width * animated
                    if (w > 0f) {
                        drawRoundRect(brush = fill, size = Size(w, size.height), cornerRadius = r)
                    }
                },
        )
    }
}

/** The titles as overlapping tiles: a glance at what is being played. */
@Composable
private fun TitleStrip(titles: List<FarmTitle>) {
    val c = Xau.colors
    val shown = titles.take(8)
    val tile = 26.dp
    val step = 13.dp
    Box(Modifier.width(tile + step * (shown.size - 1)).height(tile)) {
        shown.forEachIndexed { i, t ->
            Box(
                Modifier
                    .offset(x = step * i)
                    .size(tile)
                    .clip(RoundedCornerShape(7.dp))
                    .background(c.ink)
                    .padding(1.5.dp),
            ) {
                GameArt(t, Modifier.fillMaxSize(), corner = 6.dp)
            }
        }
    }
}

/**
 * Why the farm's last ping did not register, in the user's words; null
 * while Xbox accepts it (or before the first ping).
 *
 * A 403 is what an account signed in before the Windows sign-in gets:
 * its token's device is not a Windows device. Signing in again fixes it.
 */
internal fun farmPresenceProblem(status: String?, http: Int?): String? {
    val code = http?.let { " ($it)" }.orEmpty()
    return when {
        status == "denied" && http == 403 ->
            "Xbox recusou a presença (403) — remova a conta e entre de novo"
        else -> farmPresenceStatus(status, code)
    }
}

private fun farmPresenceStatus(status: String?, code: String): String? =
    when (status) {
        "denied" -> "Xbox recusou a presença$code — o farm não está contando"
        "rejected" -> "Xbox rejeitou a presença$code — o farm não está contando"
        "not_sent" -> "Presença não enviada — sem conexão com a Xbox"
        "unknown" -> "Xbox não confirmou a presença$code"
        else -> null
    }

/** Seconds as `mm:ss`, counting down. */
private fun clock(seconds: Long): String {
    val m = seconds / 60
    val s = seconds % 60
    return "%d:%02d".format(m, s)
}

// ----------------------------------------------------------- dialog

@Composable
private fun RemovalDialog(target: Account, onConfirm: () -> Unit, onDismiss: () -> Unit) {
    val c = Xau.colors
    androidx.compose.material3.AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = c.surfaceHigh,
        shape = RoundedCornerShape(Radius.card),
        title = { Text("Remover ${target.gamertag}?", style = Xau.type.section, color = c.textPrimary) },
        text = {
            Text(
                "A sessão sai deste aparelho e o farm desta conta para. " +
                    "Os pontos já resgatados não são afetados.",
                style = Xau.type.body,
                color = c.textSecondary,
            )
        },
        confirmButton = {
            androidx.compose.material3.TextButton(onClick = onConfirm) {
                Text("Remover", style = Xau.type.button, color = c.danger)
            }
        },
        dismissButton = {
            androidx.compose.material3.TextButton(onClick = onDismiss) {
                Text("Cancelar", style = Xau.type.button, color = c.textSecondary)
            }
        },
    )
}
