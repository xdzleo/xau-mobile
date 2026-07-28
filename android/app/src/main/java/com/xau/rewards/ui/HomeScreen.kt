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
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import coil.compose.AsyncImage
import com.xau.rewards.R
import com.xau.rewards.UiState
import com.xau.rewards.core.Account
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau

/**
 * The home screen — the "Guide" direction. See `docs/GUIDE-spec.md`.
 *
 * The shape of this screen is the argument: **full bleed, 0dp corners,
 * no card anywhere.** Blocks are objects because `slab` clears `carbon`
 * by 1.65:1, not because a border draws them. The previous version's
 * 1.09:1 fill is why every container there needed an outline.
 *
 * The hero is a **state, not a number**. From across a room the app is
 * dark or it is a bright bar, and that answers the ten-second question
 * before a glyph is read. The band *is* the claim control, which
 * removes the giant-number-plus-green-button pair entirely.
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

    LazyColumn(
        Modifier
            .fillMaxSize()
            .background(c.carbon)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        item { SystemStrip(state) }
        item { VerdictBand(state, onClaim, onRetry) }
        item { BalanceRow(state) }

        item { SectionHeader("CONTAS") }
        item {
            AccountsBlock(
                state = state,
                onSelect = onSelect,
                onLongPress = { pendingRemoval = it },
                onAddAccount = onAddAccount,
            )
        }

        item { SectionHeader("PRESENÇA") }
        item { PresenceBlock(state, onToggleFarm) }

        item { Spacer(Modifier.height(Space.xxl)) }
    }
}

// ------------------------------------------------------- 1 system strip

/**
 * 36dp of `slab`, full bleed. Carries what the app knows about itself:
 * how many accounts, which address the traffic leaves by, and whether
 * the farm is transmitting. Replaces the old lockup and the orphaned IP
 * line that sat alone at the bottom of the scroll.
 */
@Composable
private fun SystemStrip(state: UiState) {
    val c = Xau.colors
    val broadcasting = state.farm?.running == true

    Row(
        Modifier
            .fillMaxWidth()
            .height(36.dp)
            .background(c.slab)
            .padding(horizontal = Space.gutter),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        androidx.compose.material3.Icon(
            painterResource(R.drawable.ic_xbox),
            contentDescription = null,
            tint = c.muted,
            modifier = Modifier.size(14.dp),
        )
        Spacer(Modifier.width(Space.sm))
        Text("XAU", style = Xau.type.header, color = c.dim)
        Spacer(Modifier.width(Space.sm))
        Text(
            "${state.accounts.size} CONTAS",
            style = Xau.type.header,
            color = c.muted,
        )

        Spacer(Modifier.weight(1f))

        state.publicIp?.let {
            Text(it, style = Xau.type.register, color = c.muted)
            Spacer(Modifier.width(Space.sm))
        }
        Box(
            Modifier
                .size(6.dp)
                .background(if (broadcasting) c.presence else c.rule),
        )
    }
}

// ------------------------------------------------------ 2 verdict band

@Composable
private fun VerdictBand(state: UiState, onClaim: (String) -> Unit, onRetry: () -> Unit) {
    val c = Xau.colors
    val haptics = LocalHapticFeedback.current

    val waiting = state.totalWaiting
    val everRead = state.accounts.any { it.balance != null }
    val failed = state.fatal != null || state.accounts.any { it.staleReason != null }

    when {
        waiting > 0 -> {
            val busy = state.claimingXuid != null
            val target = state.active ?: state.accounts.firstOrNull()
            Column(
                Modifier
                    .fillMaxWidth()
                    .heightIn(min = 112.dp)
                    .background(c.volt)
                    .then(
                        if (busy || target == null) Modifier
                        else Modifier.clickable {
                            haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                            onClaim(target.xuid)
                        },
                    )
                    .padding(Space.gutter),
                verticalArrangement = Arrangement.Center,
            ) {
                Row(
                    Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.Bottom,
                ) {
                    // Every glyph on a volt fill is carbon: 15.09:1. The
                    // value inversion is what makes this read as signage
                    // rather than as neon.
                    Text(
                        "+${money(waiting)}",
                        style = Xau.type.bandFigure,
                        color = c.onVolt,
                    )
                    Spacer(Modifier.weight(1f))
                    Text(
                        if (busy) "RESGATANDO…" else "RESGATAR",
                        style = Xau.type.bandAction,
                        color = c.onVolt,
                    )
                }
                Spacer(Modifier.height(Space.sm))
                Text(waitingDetail(state), style = Xau.type.register, color = c.onVolt)
            }
        }

        failed || !everRead -> {
            val reason = state.fatal
                ?: state.accounts.firstNotNullOfOrNull { it.staleReason }
                ?: "SEM LEITURA · TOQUE PARA TENTAR"
            BladeBand(
                blade = c.pulse,
                onTap = onRetry,
                title = if (everRead) "Leitura falhou" else "Sem leitura",
                action = "TENTAR",
                detail = reason.uppercase(),
                detailColor = c.pulse,
            )
        }

        else -> BladeBand(
            blade = c.rule,
            onTap = null,
            title = "Nada a resgatar",
            action = null,
            detail = if (state.refreshing) "LENDO…" else "TUDO RESGATADO",
            detailColor = c.muted,
        )
    }
}

/** The CLEAR and UNKNOWN bands: `slab` with a 4dp leading blade. */
@Composable
private fun BladeBand(
    blade: Color,
    onTap: (() -> Unit)?,
    title: String,
    action: String?,
    detail: String,
    detailColor: Color,
) {
    val c = Xau.colors
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = 96.dp)
            .background(c.slab)
            .then(if (onTap != null) Modifier.clickable(onClick = onTap) else Modifier),
    ) {
        Box(Modifier.width(4.dp).fillMaxSize().background(blade))
        Column(
            Modifier.padding(Space.gutter),
            verticalArrangement = Arrangement.Center,
        ) {
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    title,
                    style = Xau.type.verdict,
                    color = c.focus,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f, fill = false),
                )
                if (action != null) {
                    Spacer(Modifier.weight(1f))
                    Text(action, style = Xau.type.bandAction, color = c.focus)
                }
            }
            Spacer(Modifier.height(6.dp))
            Text(
                detail,
                style = Xau.type.register,
                color = detailColor,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

private fun waitingDetail(state: UiState): String {
    val withPoints = state.accounts.filter { (it.unclaimed ?: 0L) > 0L }
    val tags = withPoints.joinToString(" · ") { it.gamertag.uppercase() }
    val unread = state.accounts.count { it.unclaimed == null }
    val head = "EM ${withPoints.size} CONTA${if (withPoints.size == 1) "" else "S"}"
    val tail = if (unread > 0) " · $unread SEM LEITURA" else ""
    return if (tags.isEmpty()) head + tail else "$head · $tags$tail"
}

// ------------------------------------------------------- 3 balance row

/**
 * A stat line, not a card. The balance answers a question nobody opened
 * the app to ask, so it gets 44dp and a rule, not 56sp and a hero slot.
 */
@Composable
private fun BalanceRow(state: UiState) {
    val c = Xau.colors
    val unread = state.accounts.count { it.balance == null }
    // A total that silently sums only the accounts that answered is a
    // more confident lie than an em dash.
    val total = if (unread > 0) null else state.accounts.sumOf { it.balance ?: 0L }
    val score = state.accounts.sumOf { it.gamerscore }

    Column {
        Box(Modifier.fillMaxWidth().height(1.dp).background(c.rule))
        Row(
            Modifier
                .fillMaxWidth()
                .height(44.dp)
                .padding(horizontal = Space.gutter),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                if (state.accounts.size > 1) "SALDO TOTAL" else "SALDO",
                style = Xau.type.header,
                color = c.muted,
            )
            Spacer(Modifier.weight(1f))
            if (unread > 0) {
                Text("$unread SEM LEITURA", style = Xau.type.register, color = c.pulse)
                Spacer(Modifier.width(Space.sm))
            }
            Text(
                money(total),
                style = Xau.type.rowFigure,
                color = if (total == null) c.muted else c.focus,
            )
            Spacer(Modifier.width(Space.sm))
            Text("${money(score)} G", style = Xau.type.register, color = c.muted)
        }
        Box(Modifier.fillMaxWidth().height(1.dp).background(c.rule))
    }
}

// ---------------------------------------------------------- 4/6 header

/**
 * A table header, not a floating label: the text sits at the gutter and
 * a rule runs from the end of it to the screen edge.
 */
@Composable
private fun SectionHeader(text: String) {
    val c = Xau.colors
    Row(
        Modifier.fillMaxWidth().height(28.dp).padding(start = Space.gutter),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(text, style = Xau.type.header, color = c.muted)
        Spacer(Modifier.width(Space.md))
        Box(Modifier.weight(1f).height(1.dp).background(c.rule))
    }
}

// -------------------------------------------------------- 5 accounts

@Composable
private fun AccountsBlock(
    state: UiState,
    onSelect: (String) -> Unit,
    onLongPress: (Account) -> Unit,
    onAddAccount: () -> Unit,
) {
    val c = Xau.colors
    Column(Modifier.fillMaxWidth().background(c.slab)) {
        if (state.booting) {
            repeat(2) {
                AccountRowSkeleton()
                Box(Modifier.fillMaxWidth().height(1.dp).background(c.rule))
            }
        } else {
            state.accounts.forEachIndexed { i, a ->
                if (i > 0) Box(Modifier.fillMaxWidth().height(1.dp).background(c.rule))
                AccountRow(a, onSelect = { onSelect(a.xuid) }, onLongPress = { onLongPress(a) })
            }
            if (state.accounts.isNotEmpty()) {
                Box(Modifier.fillMaxWidth().height(1.dp).background(c.rule))
            }
        }

        Box(
            Modifier
                .fillMaxWidth()
                .height(56.dp)
                .clickable(onClick = onAddAccount)
                .padding(horizontal = Space.gutter),
            contentAlignment = Alignment.CenterStart,
        ) {
            Text("+ ADICIONAR CONTA", style = Xau.type.blockAction, color = c.dim)
        }
    }
}

@Composable
private fun AccountRow(account: Account, onSelect: () -> Unit, onLongPress: () -> Unit) {
    val c = Xau.colors
    val waiting = account.unclaimed ?: 0L
    val stale = account.staleReason != null

    // Precedence, highest first: money on the table, then a failed read,
    // then broadcasting, then nothing.
    val blade = when {
        waiting > 0 -> c.volt
        stale -> c.pulse
        account.farming -> c.presence
        else -> c.rule
    }

    Box(
        Modifier
            .fillMaxWidth()
            .height(64.dp)
            .background(if (account.active) c.slabHigh else c.slab)
            // The only border in the app. 2dp of focus at 8.08:1 — the
            // previous marker was a 1.10:1 fill change, which is to say
            // no marker at all.
            .then(
                if (account.active) Modifier.border(2.dp, c.focus)
                else Modifier,
            )
            // No scale on press: a 0.97 scale on a full-bleed row leaves
            // a gap at the screen edge and reads as breakage.
            .pressableRow(onClick = onSelect, onLongClick = onLongPress),
    ) {
        Row(Modifier.fillMaxSize(), verticalAlignment = Alignment.CenterVertically) {
            Box(Modifier.width(4.dp).fillMaxSize().background(blade))
            Spacer(Modifier.width(Space.md))
            Gamerpic(account)
            Spacer(Modifier.width(Space.md))

            Column(Modifier.weight(1f)) {
                Text(
                    account.gamertag.ifEmpty { account.xuid },
                    style = Xau.type.rowTag,
                    color = c.focus,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.height(2.dp))
                Text(
                    account.staleReason ?: "${money(account.gamerscore)} G",
                    style = Xau.type.register,
                    color = if (stale) c.pulse else c.muted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }

            Column(horizontalAlignment = Alignment.End) {
                Text(
                    money(account.balance),
                    style = Xau.type.rowFigure,
                    color = when {
                        stale -> c.pulse
                        account.balance == null -> c.muted
                        else -> c.focus
                    },
                )
                Spacer(Modifier.height(2.dp))
                when {
                    waiting > 0 -> Text(
                        "+${money(waiting)}",
                        style = Xau.type.rowDelta,
                        color = c.volt,
                    )
                    account.farming -> Text("TX", style = Xau.type.register, color = c.presence)
                }
            }
            Spacer(Modifier.width(Space.gutter))
        }
    }
}

/**
 * A square gamerpic. The circle is the phone tell; console gamerpics
 * are square, and that one decision carries more Xbox than any amount
 * of green would.
 */
@Composable
private fun Gamerpic(account: Account) {
    val c = Xau.colors
    Box(
        Modifier.size(40.dp).background(c.slabHigh),
        contentAlignment = Alignment.Center,
    ) {
        if (account.gamerpic.isNotEmpty()) {
            AsyncImage(
                model = account.gamerpic,
                contentDescription = null,
                modifier = Modifier.fillMaxSize(),
            )
        } else {
            Text(
                account.gamertag.take(1).uppercase(),
                style = Xau.type.rowTag,
                color = c.dim,
            )
        }
    }
}

@Composable
private fun AccountRowSkeleton() {
    val c = Xau.colors
    Row(
        Modifier.fillMaxWidth().height(64.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.width(4.dp).fillMaxSize().background(c.rule))
        Spacer(Modifier.width(Space.md))
        Box(Modifier.size(40.dp).background(c.slabHigh))
        Spacer(Modifier.width(Space.md))
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(Space.xs)) {
            DotLeader(120.dp)
            DotLeader(80.dp)
        }
        Spacer(Modifier.width(Space.gutter))
    }
}

// ------------------------------------------------------- 7 presence

@Composable
private fun PresenceBlock(state: UiState, onToggle: () -> Unit) {
    val c = Xau.colors
    val haptics = LocalHapticFeedback.current
    val farm = state.farm
    val broadcasting = farm?.running == true

    // The Farming/Pinging split the core already reports and the old UI
    // collapsed into a boolean and threw away.
    val (word, wordColor) = when (farm?.state) {
        "Pinging" -> "TRANSMITINDO" to c.presence
        "Farming" -> "EM CICLO" to c.focus
        "Error" -> "ERRO" to c.pulse
        else -> "PARADO" to c.muted
    }

    Column(Modifier.fillMaxWidth().background(c.slab)) {
        Row(
            Modifier.fillMaxWidth().height(56.dp).padding(horizontal = Space.gutter),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(word, style = Xau.type.stateWord, color = wordColor)
            Spacer(Modifier.weight(1f))
            Rocker(engaged = broadcasting) {
                haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                onToggle()
            }
        }

        Box(Modifier.fillMaxWidth().height(1.dp).background(c.rule))

        // Two Rows of four, not a LazyVerticalGrid: eight items are known
        // at compile time, and a lazy grid inside a scrolling column
        // crashes on infinite height constraints.
        Column {
            FARM_TILES.chunked(4).forEach { row ->
                Row(
                    Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(2.dp),
                ) {
                    row.forEach { (id, name) ->
                        Tile(id, name, broadcasting, Modifier.weight(1f))
                    }
                }
                Spacer(Modifier.height(2.dp))
            }
        }

        if (broadcasting && farm != null) {
            Box(Modifier.fillMaxWidth().height(1.dp).background(c.rule))
            Row(
                Modifier.fillMaxWidth().height(36.dp).padding(horizontal = Space.gutter),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("CICLO ${farm.cycle}", style = Xau.type.register, color = c.muted)
                Spacer(Modifier.weight(1f))
                Text(
                    "${farm.elapsedSecs / 60} DE ${farm.totalSecs / 60} MIN",
                    style = Xau.type.register,
                    color = c.muted,
                )
            }
        }
    }
}

/**
 * A two-position plate rather than a Material switch. The pill is the
 * phone tell; this is a control on a panel.
 */
@Composable
private fun Rocker(engaged: Boolean, onToggle: () -> Unit) {
    val c = Xau.colors
    Row(
        Modifier
            .size(width = 112.dp, height = 48.dp)
            .clip(RoundedCornerShape(Radius.seam))
            .clickable(
                role = Role.Switch,
                onClick = onToggle,
            ),
    ) {
        RockerHalf("DESL", active = !engaged, Modifier.weight(1f))
        RockerHalf("TX", active = engaged, Modifier.weight(1f))
    }
}

@Composable
private fun RockerHalf(label: String, active: Boolean, modifier: Modifier) {
    val c = Xau.colors
    Column(
        modifier
            .fillMaxSize()
            .background(if (active) c.slabHigh else c.slab),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Spacer(Modifier.weight(1f))
        Text(
            label,
            style = Xau.type.stateWord,
            color = if (active) c.focus else c.muted,
        )
        Spacer(Modifier.weight(1f))
        Box(
            Modifier
                .fillMaxWidth()
                .height(3.dp)
                .background(if (active) c.presence else Color.Transparent),
        )
    }
}

@Composable
private fun Tile(titleId: String, name: String, broadcasting: Boolean, modifier: Modifier) {
    val c = Xau.colors
    Column(
        modifier
            .heightIn(min = 72.dp)
            .background(c.slabHigh)
            .padding(Space.sm),
    ) {
        Text(titleId, style = Xau.type.tileId, color = c.muted)
        Spacer(Modifier.weight(1f))
        Text(
            name,
            style = Xau.type.tileName,
            color = if (broadcasting) c.dim else c.muted,
            maxLines = 2,
            overflow = TextOverflow.Ellipsis,
        )
        Spacer(Modifier.height(Space.xs))
        // All eight light together, because send_heartbeat_bulk posts
        // all eight title IDs in one call. A visual implying rotation
        // would be decoration wearing a data costume.
        Box(
            Modifier
                .fillMaxWidth()
                .height(3.dp)
                .background(if (broadcasting) c.presence else Color.Transparent),
        )
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
        containerColor = c.slab,
        shape = RoundedCornerShape(Radius.none),
        title = {
            Text("Remover ${target.gamertag}?", style = Xau.type.verdict, color = c.focus)
        },
        text = {
            Text(
                "A sessão sai deste aparelho e o farm desta conta para. " +
                    "Os pontos já resgatados não são afetados.",
                style = Xau.type.body,
                color = c.dim,
            )
        },
        confirmButton = {
            androidx.compose.material3.TextButton(onClick = onConfirm) {
                Text("REMOVER", style = Xau.type.bandAction, color = c.pulse)
            }
        },
        dismissButton = {
            androidx.compose.material3.TextButton(onClick = onDismiss) {
                Text("CANCELAR", style = Xau.type.bandAction, color = c.dim)
            }
        },
    )
}
