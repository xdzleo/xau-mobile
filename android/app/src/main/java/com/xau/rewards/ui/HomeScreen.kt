package com.xau.rewards.ui

import androidx.compose.foundation.Canvas
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
import androidx.compose.foundation.lazy.LazyRow
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
 * The home screen, built to the supplied mockup.
 *
 * Top to bottom: brand lockup with a level pill, a "Resumo de hoje"
 * card pairing the day's minutes with a completion ring, the eight
 * presence titles as toggle rows, a horizontal strip of managed
 * accounts, and a bottom tab bar.
 */
@Composable
fun HomeScreen(
    state: UiState,
    onClaim: (String) -> Unit,
    onSelect: (String) -> Unit,
    onSignOut: (String) -> Unit,
    onAddAccount: () -> Unit,
    onToggleFarm: () -> Unit,
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
            item { TodayCard(state, onRetry) }

            item {
                SectionHead(
                    icon = R.drawable.ic_pad,
                    title = "Presença em jogos",
                    subtitle = "Gerencie sua presença nos títulos elegíveis",
                    actionLabel = "Atualizar tudo",
                    actionIcon = R.drawable.ic_refresh,
                    onAction = onRetry,
                )
            }
            item { GamesBlock(state, onToggleFarm) }

            item {
                SectionHead(
                    icon = R.drawable.ic_people,
                    title = "Contas gerenciadas",
                    subtitle = "Alterne entre suas contas",
                    actionLabel = "Gerenciar contas",
                    actionIcon = R.drawable.ic_gear,
                    onAction = {},
                )
            }
            item {
                AccountStrip(
                    state = state,
                    onSelect = onSelect,
                    onLongPress = { pendingRemoval = it },
                    onAddAccount = onAddAccount,
                )
            }

            state.notice?.let { item { Text(it, style = Xau.type.caption, color = c.textMuted) } }
        }

        TabBar()
    }
}

// --------------------------------------------------------------- header

@Composable
private fun Header(state: UiState) {
    val c = Xau.colors
    val account = state.active

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

        LevelPill(account)
    }
}

@Composable
private fun LevelPill(account: Account?) {
    val c = Xau.colors
    Row(
        Modifier
            .clip(RoundedCornerShape(Radius.control))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.control))
            .padding(horizontal = Space.md, vertical = Space.sm),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column {
            Text("Nível ${account?.level ?: "—"}", style = Xau.type.caption, color = c.textSecondary)
            Text(money(account?.balance), style = Xau.type.rowTitle, color = c.textPrimary)
        }
        Spacer(Modifier.width(Space.sm))
        Box(
            Modifier
                .size(28.dp)
                .clip(RoundedCornerShape(Radius.pill))
                .border(1.5.dp, c.green, RoundedCornerShape(Radius.pill)),
            contentAlignment = Alignment.Center,
        ) {
            Text("XP", style = Xau.type.captionStrong, color = c.green)
        }
        Spacer(Modifier.width(Space.xs))
        Text("›", style = Xau.type.section, color = c.textSecondary)
    }
}

// ----------------------------------------------------------- today card

@Composable
private fun TodayCard(state: UiState, onRefresh: () -> Unit) {
    val c = Xau.colors
    val farm = state.farm
    val active = if (farm?.running == true) FARM_TILES.size else 0
    val minutes = (farm?.elapsedSecs ?: 0L) / 60 + (farm?.cycle ?: 0L) * 16

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
                    if (state.booting) "—" else money(minutes),
                    style = Xau.type.hero,
                    color = c.textPrimary,
                )
                Text("minutos acumulados", style = Xau.type.body, color = c.green)
                Spacer(Modifier.height(Space.sm))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        if (state.refreshing) "Atualizando…" else "Atualizado agora há pouco",
                        style = Xau.type.caption,
                        color = c.textMuted,
                    )
                    Spacer(Modifier.width(Space.sm))
                    Box(
                        Modifier
                            .size(6.dp)
                            .clip(RoundedCornerShape(Radius.pill))
                            .background(if (state.fatal == null) c.green else c.danger),
                    )
                }
                Spacer(Modifier.height(Space.lg))
                GhostButton("Ver estatísticas", R.drawable.ic_bars, onRefresh)
            }

            Spacer(Modifier.width(Space.lg))

            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                ProgressRing(active, FARM_TILES.size)
                Spacer(Modifier.height(Space.md))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        if (active == FARM_TILES.size) "Meta diária concluída!" else "Meta diária pendente",
                        style = Xau.type.caption,
                        color = c.textSecondary,
                        textAlign = TextAlign.Center,
                    )
                    if (active == FARM_TILES.size) {
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
                topLeft = androidx.compose.ui.geometry.Offset(inset, inset),
                size = arcSize,
                style = Stroke(width = stroke, cap = StrokeCap.Round),
            )
            if (fraction > 0f) {
                drawArc(
                    color = c.green,
                    startAngle = -90f,
                    sweepAngle = 360f * fraction,
                    useCenter = false,
                    topLeft = androidx.compose.ui.geometry.Offset(inset, inset),
                    size = arcSize,
                    style = Stroke(width = stroke, cap = StrokeCap.Round),
                )
            }
        }
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            Text("$done/$total", style = Xau.type.ringFigure, color = c.textPrimary)
            Text("Títulos ativos", style = Xau.type.caption, color = c.textSecondary)
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

// ---------------------------------------------------------------- games

@Composable
private fun GamesBlock(state: UiState, onToggleFarm: () -> Unit) {
    val on = state.farm?.running == true
    Column(verticalArrangement = Arrangement.spacedBy(Space.sm)) {
        FARM_TILES.forEach { (id, name) ->
            GameRow(id, name, on, onToggleFarm)
        }
    }
}

@Composable
private fun GameRow(titleId: String, name: String, on: Boolean, onToggle: () -> Unit) {
    val c = Xau.colors
    val haptics = LocalHapticFeedback.current

    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.row))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.row))
            .padding(Space.md),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        // Xbox does not hand us box art for a title id, so the slot the
        // mockup fills with cover art gets the initial instead. A broken
        // image icon in a list of eight would read as a bug.
        Box(
            Modifier
                .size(48.dp)
                .clip(RoundedCornerShape(Radius.control))
                .background(c.surfaceHigh),
            contentAlignment = Alignment.Center,
        ) {
            Text(name.take(1), style = Xau.type.rowTitle, color = c.textSecondary)
        }

        Spacer(Modifier.width(Space.md))

        Column(Modifier.weight(1f)) {
            Text(
                name,
                style = Xau.type.rowTitle,
                color = c.textPrimary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(2.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    Modifier
                        .size(6.dp)
                        .clip(RoundedCornerShape(Radius.pill))
                        .background(if (on) c.green else c.textMuted),
                )
                Spacer(Modifier.width(Space.sm))
                Text(
                    if (on) "Presença ativa" else "Presença pausada",
                    style = Xau.type.caption,
                    color = if (on) c.green else c.textMuted,
                )
            }
        }

        Column(horizontalAlignment = Alignment.End) {
            Text(if (on) "60 min" else "0 min", style = Xau.type.rowFigure, color = c.textPrimary)
            Text("Máx. diário: 60 min", style = Xau.type.caption, color = c.textMuted)
        }

        Spacer(Modifier.width(Space.md))

        Switch(
            checked = on,
            onCheckedChange = {
                haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                onToggle()
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
}

// ------------------------------------------------------------ accounts

@Composable
private fun AccountStrip(
    state: UiState,
    onSelect: (String) -> Unit,
    onLongPress: (Account) -> Unit,
    onAddAccount: () -> Unit,
) {
    LazyRow(horizontalArrangement = Arrangement.spacedBy(Space.md)) {
        items(state.accounts.size) { i ->
            val a = state.accounts[i]
            AccountCard(a, onSelect = { onSelect(a.xuid) }, onLongPress = { onLongPress(a) })
        }
        item { AddAccountCard(onAddAccount) }
    }
}

@Composable
private fun AccountCard(account: Account, onSelect: () -> Unit, onLongPress: () -> Unit) {
    val c = Xau.colors
    Box(
        Modifier
            .width(124.dp)
            .clip(RoundedCornerShape(Radius.row))
            .background(c.surface)
            .border(
                if (account.active) 1.5.dp else 1.dp,
                if (account.active) c.green else c.outline,
                RoundedCornerShape(Radius.row),
            )
            .pressableRow(onClick = onSelect, onLongClick = onLongPress)
            .padding(vertical = Space.lg, horizontal = Space.md),
    ) {
        if (account.active) {
            Icon(
                painterResource(R.drawable.ic_check),
                contentDescription = null,
                tint = c.green,
                modifier = Modifier.size(16.dp).align(Alignment.TopEnd),
            )
        }
        Column(
            Modifier.fillMaxWidth(),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Box(
                Modifier
                    .size(56.dp)
                    .clip(RoundedCornerShape(Radius.pill))
                    .background(c.surfaceHigh),
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
                        style = Xau.type.section,
                        color = c.textSecondary,
                    )
                }
            }
            Spacer(Modifier.height(Space.sm))
            Text(
                account.gamertag.ifEmpty { account.xuid },
                style = Xau.type.captionStrong,
                color = c.textPrimary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                account.level?.let { "Nível $it" } ?: "—",
                style = Xau.type.caption,
                color = c.green,
            )
        }
    }
}

@Composable
private fun AddAccountCard(onClick: () -> Unit) {
    val c = Xau.colors
    Column(
        Modifier
            .width(124.dp)
            .clip(RoundedCornerShape(Radius.row))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.row))
            .clickable(onClick = onClick)
            .padding(vertical = Space.lg, horizontal = Space.md),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Box(
            Modifier
                .size(56.dp)
                .clip(RoundedCornerShape(Radius.pill))
                .background(c.surfaceHigh),
            contentAlignment = Alignment.Center,
        ) {
            Text("+", style = Xau.type.hero.copy(fontSize = Xau.type.section.fontSize), color = c.textSecondary)
        }
        Spacer(Modifier.height(Space.sm))
        Text(
            "Adicionar\nconta",
            style = Xau.type.caption,
            color = c.textSecondary,
            textAlign = TextAlign.Center,
        )
    }
}

// -------------------------------------------------------------- tabbar

@Composable
private fun TabBar() {
    val c = Xau.colors
    val tabs = listOf(
        R.drawable.ic_home to "Início",
        R.drawable.ic_books to "Biblioteca",
        R.drawable.ic_bars to "Atividade",
        R.drawable.ic_gear to "Configurações",
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
        tabs.forEachIndexed { i, (icon, label) ->
            val selected = i == 0
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                Icon(
                    painterResource(icon),
                    contentDescription = label,
                    tint = if (selected) c.green else c.textMuted,
                    modifier = Modifier.size(22.dp),
                )
                Spacer(Modifier.height(Space.xs))
                Text(
                    label,
                    style = Xau.type.caption,
                    color = if (selected) c.green else c.textMuted,
                )
            }
        }
    }
}

/** Mirrors `FIXED_CONSOLE_TITLES` in the core, in the core's order. */
private val FARM_TILES = listOf(
    "331135949" to "Doom Eternal",
    "1790482525" to "Call of Duty: Black Ops Cold War",
    "2043073184" to "Halo Infinite",
    "2030093255" to "Forza Horizon 5",
    "374923716" to "Gears 5",
    "2079794073" to "Starfield",
    "1842668909" to "Forza Motorsport",
    "66591171" to "Sea of Thieves",
)

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
