package com.xau.rewards.ui

import androidx.compose.foundation.Canvas
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.runtime.LaunchedEffect
import kotlinx.coroutines.delay
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
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import coil.compose.AsyncImage
import com.xau.rewards.R
import com.xau.rewards.UiState
import com.xau.rewards.core.Account
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau

/**
 * The home screen: accounts, and nothing else.
 *
 * The per-title presence list is gone. It listed eight fixed titles that
 * the user cannot choose and that every account farms identically — a
 * screenful of rows carrying one bit of information between them. The
 * farm switch belongs on the account, because that is the thing there is
 * more than one of and the thing the core actually keys farms by.
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

    Column(
        Modifier
            .fillMaxSize()
            .background(c.ink)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        LazyColumn(
            Modifier.weight(1f).fillMaxWidth(),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(
                start = Space.gutter,
                end = Space.gutter,
                top = Space.md,
                bottom = Space.xxl,
            ),
            verticalArrangement = Arrangement.spacedBy(Space.lg),
        ) {
            item { Header(state) }
            item { TodayCard(state) }

            item {
                SectionHead(
                    icon = R.drawable.ic_people,
                    title = "Contas gerenciadas",
                    subtitle = "Ligue para upar cada conta",
                    actionLabel = "Atualizar",
                    actionIcon = R.drawable.ic_refresh,
                    onAction = onRetry,
                )
            }

            if (state.booting) {
                items(2) { AccountRowSkeleton() }
            } else {
                items(state.accounts.size) { i ->
                    val a = state.accounts[i]
                    AccountRow(
                        account = a,
                        claiming = state.claimingXuid == a.xuid,
                        onSelect = { onSelect(a.xuid); onOpenAccount(a) },
                        onLongPress = { pendingRemoval = a },
                        onToggleFarm = { onToggleFarm(a.xuid) },
                        onClaim = { onClaim(a.xuid) },
                    )
                }
                item { AddAccountRow(onAddAccount) }
            }
        }

        TabBar(onOpenOffers)
    }
}

// --------------------------------------------------------------- header

@Composable
private fun Header(state: UiState) {
    val c = Xau.colors
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Box(
            Modifier.size(44.dp).clip(RoundedCornerShape(Radius.pill)).background(Color.White),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painterResource(R.drawable.ic_xbox),
                contentDescription = null,
                tint = c.greenDeep,
                modifier = Modifier.size(30.dp),
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
            Text(
                "Jogos. Presença. Recompensas.",
                style = Xau.type.caption,
                color = c.textSecondary,
            )
        }
        // The total across every account, which is the only figure that
        // belongs in a header when the app is a multi-account tool.
        Column(horizontalAlignment = Alignment.End) {
            Text("TOTAL", style = Xau.type.caption, color = c.textMuted)
            Text(money(state.totalBalance), style = Xau.type.rowFigure, color = c.textPrimary)
        }
    }
}

// ----------------------------------------------------------- today card

/**
 * The day's summary, expressed as level progress across the accounts.
 *
 * Rewards level is the thing being farmed for, so "how many are still on
 * level 1" is the question the summary should answer. Accounts whose
 * level could not be read are counted separately and never folded into
 * level 1 — those are different facts.
 */
@Composable
private fun TodayCard(state: UiState) {
    val c = Xau.colors
    val lv1 = state.accounts.count { it.level == 1L }
    val up = state.accounts.count { (it.level ?: 0L) >= 2L }
    val unknown = state.accounts.count { it.level == null }
    val farming = state.accounts.count { it.farming }
    val withGp = state.accounts.count { it.gamepass != null }
    val total = state.accounts.size

    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.card))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.card))
            .padding(Space.xl),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(
                painterResource(R.drawable.ic_clock),
                contentDescription = null,
                tint = c.green,
                modifier = Modifier.size(18.dp),
            )
            Spacer(Modifier.width(Space.sm))
            Text("Resumo de hoje", style = Xau.type.section, color = c.textPrimary)
        }

        Spacer(Modifier.height(Space.lg))

        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(
                    if (state.booting) "—" else "$up",
                    style = Xau.type.hero,
                    color = c.textPrimary,
                )
                Text(
                    if (up == 1) "conta upada" else "contas upadas",
                    style = Xau.type.body,
                    color = c.green,
                )
                Spacer(Modifier.height(Space.md))

                LevelTally("Nível 1", lv1, c.textSecondary)
                Spacer(Modifier.height(Space.xs))
                LevelTally("Nível 2 ou 3", up, c.green)
                if (withGp > 0 || total > 0) {
                    Spacer(Modifier.height(Space.xs))
                    LevelTally("com Game Pass", withGp, c.green)
                }
                if (unknown > 0) {
                    Spacer(Modifier.height(Space.xs))
                    // Never folded into level 1: "has not levelled up"
                    // and "could not be read" are different facts, and
                    // the whole point of this card is to tell them apart.
                    LevelTally("Sem leitura", unknown, c.textMuted)
                }
            }

            Spacer(Modifier.width(Space.lg))

            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                ProgressRing(farming, total)
                Spacer(Modifier.height(Space.md))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        if (farming == total && total > 0) "Todas farmando" else "Farmando",
                        style = Xau.type.caption,
                        color = c.textSecondary,
                        textAlign = TextAlign.Center,
                    )
                    if (farming == total && total > 0) {
                        Spacer(Modifier.width(Space.xs))
                        Icon(
                            painterResource(R.drawable.ic_check),
                            contentDescription = null,
                            tint = c.green,
                            modifier = Modifier.size(16.dp),
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun LevelTally(label: String, count: Int, tint: Color) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(
            Modifier.size(6.dp).clip(RoundedCornerShape(Radius.pill)).background(tint),
        )
        Spacer(Modifier.width(Space.sm))
        Text("$count", style = Xau.type.captionStrong, color = tint)
        Spacer(Modifier.width(Space.xs))
        Text(label, style = Xau.type.caption, color = Xau.colors.textSecondary)
    }
}

@Composable
private fun ProgressRing(done: Int, total: Int) {
    val c = Xau.colors
    val fraction = if (total == 0) 0f else done.toFloat() / total
    Box(Modifier.size(132.dp), contentAlignment = Alignment.Center) {
        Canvas(Modifier.fillMaxSize()) {
            val stroke = 12.dp.toPx()
            val inset = stroke / 2
            val arcSize = Size(size.width - stroke, size.height - stroke)
            drawArc(
                color = c.surfaceHigh,
                startAngle = 0f,
                sweepAngle = 360f,
                useCenter = false,
                topLeft = Offset(inset, inset),
                size = arcSize,
                style = Stroke(width = stroke, cap = StrokeCap.Round),
            )
            if (fraction > 0f) {
                drawArc(
                    color = c.green,
                    startAngle = -90f,
                    sweepAngle = 360f * fraction,
                    useCenter = false,
                    topLeft = Offset(inset, inset),
                    size = arcSize,
                    style = Stroke(width = stroke, cap = StrokeCap.Round),
                )
            }
        }
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            Text("$done/$total", style = Xau.type.ringFigure, color = c.textPrimary)
            Text("Contas", style = Xau.type.caption, color = c.textSecondary)
        }
    }
}

// -------------------------------------------------------- section head

@Composable
private fun SectionHead(
    icon: Int,
    title: String,
    subtitle: String,
    actionLabel: String,
    actionIcon: Int,
    onAction: () -> Unit,
) {
    val c = Xau.colors
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Icon(
            painterResource(icon),
            contentDescription = null,
            tint = c.green,
            modifier = Modifier.size(20.dp),
        )
        Spacer(Modifier.width(Space.md))
        Column(Modifier.weight(1f)) {
            Text(title, style = Xau.type.section, color = c.textPrimary)
            Text(
                subtitle,
                style = Xau.type.caption,
                color = c.textSecondary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Spacer(Modifier.width(Space.sm))
        GhostButton(actionLabel, actionIcon, onAction, tint = c.green)
    }
}

@Composable
private fun GhostButton(label: String, icon: Int, onClick: () -> Unit, tint: Color? = null) {
    val c = Xau.colors
    Row(
        Modifier
            .clip(RoundedCornerShape(Radius.control))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.control))
            .clickable(onClick = onClick)
            .padding(horizontal = Space.lg, vertical = Space.md),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            painterResource(icon),
            contentDescription = null,
            tint = tint ?: c.textSecondary,
            modifier = Modifier.size(16.dp),
        )
        Spacer(Modifier.width(Space.sm))
        Text(label, style = Xau.type.button, color = c.textPrimary, maxLines = 1)
    }
}

// ------------------------------------------------------------ accounts

@Composable
private fun AccountRow(
    account: Account,
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

    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.row))
            .background(if (account.active) c.surfaceHigh else c.surface)
            .border(
                if (account.active) 1.5.dp else 1.dp,
                if (account.active) c.green else c.outline,
                RoundedCornerShape(Radius.row),
            )
            .pressableRow(onClick = onSelect, onLongClick = onLongPress)
            .padding(Space.lg),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Box(
                Modifier.size(48.dp).clip(RoundedCornerShape(Radius.pill)).background(c.surfaceHigh),
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
                    account.gamertag.ifEmpty { account.xuid },
                    style = Xau.type.rowTitle,
                    color = c.textPrimary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.height(Space.xs))
                // Badges on their own line and the mission count below.
                // Crowding all three onto one row let the Ultimate badge
                // squeeze the count down to "8 p...", which is the kind
                // of truncation that only shows up on a real handset.
                Row(verticalAlignment = Alignment.CenterVertically) {
                    LevelBadge(account.level, stale)
                    account.gamepass?.let {
                        Spacer(Modifier.width(Space.sm))
                        GamePassBadge(it)
                    }
                }
                account.email?.let {
                    Spacer(Modifier.height(Space.xs))
                    Text(
                        it,
                        style = Xau.type.caption,
                        color = c.textMuted,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                account.missionsOpen?.takeIf { it > 0 }?.let {
                    Spacer(Modifier.height(Space.xs))
                    Text(
                        "$it missões pendentes",
                        style = Xau.type.caption,
                        color = c.textSecondary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }

            Column(horizontalAlignment = Alignment.End) {
                Text(
                    money(account.balance),
                    style = Xau.type.rowFigure,
                    color = if (stale) c.danger else c.textPrimary,
                )
                Text("pontos", style = Xau.type.caption, color = c.textMuted)
            }

            Spacer(Modifier.width(Space.md))

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

        // The mission is "play 8 titles for 15 minutes"; the farm
        // broadcasts all eight at once, so one cycle IS the mission and
        // its progress is the mission's progress. The bar lives inside
        // the account banner because the farm is per account.
        if (account.farming) {
            Spacer(Modifier.height(Space.md))
            MissionProgress(account)
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
            .padding(horizontal = Space.sm, vertical = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(5.dp).clip(RoundedCornerShape(Radius.pill)).background(tint))
        Spacer(Modifier.width(Space.xs))
        Text("Nível ${level ?: "—"}", style = Xau.type.captionStrong, color = tint)
    }
}

/**
 * Game Pass tier, when the account has one.
 *
 * Read from `gamePassMembership` in the same programInfo response the
 * balance comes from, so it costs no extra request. Absent means no
 * membership — which is exactly why one of these accounts sits at level
 * 1 and cannot reach the Game Pass missions.
 */
@Composable
private fun GamePassBadge(tier: String) {
    val c = Xau.colors
    Row(
        Modifier
            .clip(RoundedCornerShape(Radius.pill))
            .background(c.greenDeep)
            .padding(horizontal = Space.sm, vertical = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(prettyTier(tier), style = Xau.type.captionStrong, color = c.onGreen)
    }
}

/** `GamePassUltimate` -> `Ultimate`, `PCGamePass` -> `PC`. */
private fun prettyTier(tier: String): String = when {
    tier.contains("Ultimate", true) -> "Ultimate"
    tier.contains("PC", true) -> "GP PC"
    tier.contains("Console", true) -> "GP Console"
    tier.contains("Core", true) -> "GP Core"
    tier.contains("Standard", true) -> "GP Standard"
    else -> "Game Pass"
}

@Composable
private fun AccountRowSkeleton() {
    val c = Xau.colors
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.row))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.row))
            .padding(Space.lg),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(48.dp).clip(RoundedCornerShape(Radius.pill)).background(c.surfaceHigh))
        Spacer(Modifier.width(Space.md))
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(Space.xs)) {
            DotLeader(140.dp)
            DotLeader(90.dp)
        }
    }
}

@Composable
private fun AddAccountRow(onClick: () -> Unit) {
    val c = Xau.colors
    Box(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.row))
            .border(1.dp, c.outline, RoundedCornerShape(Radius.row))
            .clickable(onClick = onClick)
            .padding(vertical = Space.lg),
        contentAlignment = Alignment.Center,
    ) {
        Text("+ Adicionar conta", style = Xau.type.button, color = c.textSecondary)
    }
}

// -------------------------------------------------------------- tabbar

@Composable
private fun TabBar(onOpenOffers: () -> Unit) {
    val c = Xau.colors
    // Only the first two lead anywhere; the rest are drawn but inert,
    // and are marked as such rather than pretending to work.
    val tabs = listOf(
        Triple(R.drawable.ic_home, "Início", true),
        Triple(R.drawable.ic_books, "Vantagens", true),
        Triple(R.drawable.ic_bars, "Atividade", false),
        Triple(R.drawable.ic_gear, "Configurações", false),
    )
    Row(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = Space.gutter, vertical = Space.sm)
            .clip(RoundedCornerShape(Radius.card))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.card))
            .padding(vertical = Space.md),
        horizontalArrangement = Arrangement.SpaceEvenly,
    ) {
        tabs.forEachIndexed { i, (icon, label, live) ->
            val selected = i == 0
            Column(
                horizontalAlignment = Alignment.CenterHorizontally,
                modifier = if (i == 1) Modifier.clickable(onClick = onOpenOffers) else Modifier,
            ) {
                Icon(
                    painterResource(icon),
                    contentDescription = label,
                    tint = if (selected) c.green else if (live) c.textSecondary else c.textMuted,
                    modifier = Modifier.size(22.dp),
                )
                Spacer(Modifier.height(Space.xs))
                Text(
                    label,
                    style = Xau.type.caption,
                    color = if (selected) c.green else if (live) c.textSecondary else c.textMuted,
                )
            }
        }
    }
}

/**
 * How far this account is through the 8-titles-for-15-minutes mission.
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
private fun MissionProgress(account: Account) {
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

    Column {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Text(
                "Missão de 15 min · 8 jogos",
                style = Xau.type.caption,
                color = c.textMuted,
                modifier = Modifier.weight(1f),
            )
            Text(
                "${(animated * 100).toInt()}%",
                style = Xau.type.captionStrong,
                color = c.green,
            )
            Spacer(Modifier.width(Space.sm))
            Text(
                clock(remaining),
                style = Xau.type.captionStrong,
                color = if (remaining == 0L) c.green else c.textSecondary,
            )
        }
        Spacer(Modifier.height(Space.sm))
        Box(
            Modifier
                .fillMaxWidth()
                .height(6.dp)
                .clip(RoundedCornerShape(Radius.pill))
                .background(c.surfaceHigh),
        ) {
            Box(
                Modifier
                    .fillMaxWidth(animated)
                    .height(6.dp)
                    .clip(RoundedCornerShape(Radius.pill))
                    .background(c.green),
            )
        }
    }
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
