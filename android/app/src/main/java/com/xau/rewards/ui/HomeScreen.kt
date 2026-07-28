package com.xau.rewards.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.border
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
import androidx.compose.foundation.lazy.items
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
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import coil.compose.AsyncImage
import com.xau.rewards.R
import com.xau.rewards.UiState
import com.xau.rewards.core.Account
import com.xau.rewards.core.FarmState
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau

/**
 * The home screen.
 *
 * Built for the ten-second glance described in `DESIGN.md` §1: the user
 * opens this several times a day to answer one question — *is there
 * anything to collect?* So the balance is the hero, the claim is the
 * only filled control, and everything else is context underneath.
 */
@Composable
fun HomeScreen(
    state: UiState,
    onClaim: (String) -> Unit,
    onSelect: (String) -> Unit,
    onSignOut: (String) -> Unit,
    onAddAccount: () -> Unit,
    onToggleFarm: () -> Unit,
) {
    val c = Xau.colors
    // Removing an account is destructive and rare, so it lives behind a
    // long press rather than taking permanent space on every row — with
    // a confirmation, because a long press can be a mis-grab.
    var pendingRemoval by remember { mutableStateOf<Account?>(null) }

    pendingRemoval?.let { target ->
        androidx.compose.material3.AlertDialog(
            onDismissRequest = { pendingRemoval = null },
            containerColor = c.surfaceHigh,
            title = {
                Text("Remover ${target.gamertag}?", style = Xau.type.section, color = c.textPrimary)
            },
            text = {
                Text(
                    "A sessão sai deste aparelho e o farm desta conta para. " +
                        "Os pontos já resgatados não são afetados.",
                    style = Xau.type.body,
                    color = c.textSecondary,
                )
            },
            confirmButton = {
                androidx.compose.material3.TextButton(onClick = {
                    onSignOut(target.xuid)
                    pendingRemoval = null
                }) { Text("Remover", color = c.danger, style = Xau.type.rowTitle) }
            },
            dismissButton = {
                androidx.compose.material3.TextButton(onClick = { pendingRemoval = null }) {
                    Text("Cancelar", color = c.textSecondary, style = Xau.type.rowTitle)
                }
            },
        )
    }

    LazyColumn(
        Modifier
            .fillMaxSize()
            .background(c.ink)
            .windowInsetsPadding(WindowInsets.safeDrawing),
        contentPadding = androidx.compose.foundation.layout.PaddingValues(
            start = Space.gutter,
            end = Space.gutter,
            top = Space.sm,
            bottom = Space.xxl,
        ),
        verticalArrangement = Arrangement.spacedBy(Space.md),
    ) {
        item { Lockup() }

        item {
            Spacer(Modifier.height(Space.sm))
            Hero(state, onClaim)
        }

        item {
            Spacer(Modifier.height(Space.sm))
            SectionLabel("Contas")
        }

        if (state.booting) {
            items(2) { AccountSkeleton() }
        } else {
            items(state.accounts, key = { it.xuid }) { a ->
                AccountCard(
                    account = a,
                    claiming = state.claimingXuid == a.xuid,
                    onSelect = { onSelect(a.xuid) },
                    onLongPress = { pendingRemoval = a },
                )
            }
            item { AddAccountCard(onAddAccount) }
        }

        if (state.signedIn) {
            item {
                Spacer(Modifier.height(Space.sm))
                SectionLabel("Presença")
            }
            item { FarmCard(state.farm, onToggleFarm) }
        }

        state.publicIp?.let { ip ->
            item {
                Spacer(Modifier.height(Space.sm))
                Text(
                    "Saindo por $ip",
                    style = Xau.type.caption,
                    color = c.textMuted,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        }
    }
}

// -------------------------------------------------------------- lockup

@Composable
private fun Lockup() {
    val c = Xau.colors
    Row(verticalAlignment = Alignment.CenterVertically) {
        Icon(
            painterResource(R.drawable.ic_xbox),
            contentDescription = null,
            tint = c.textPrimary,
            modifier = Modifier.size(20.dp),
        )
        Spacer(Modifier.width(Space.sm))
        Text("Xbox Rewards", style = Xau.type.rowTitle, color = c.textSecondary)
    }
}

// ---------------------------------------------------------------- hero

/**
 * The balance, at the size that says it is the product.
 *
 * The claim button appears only when something is genuinely waiting —
 * a permanently-present button that usually does nothing would train
 * the user to ignore the one moment it matters.
 */
@Composable
private fun Hero(state: UiState, onClaim: (String) -> Unit) {
    val c = Xau.colors
    val account = state.active
    val haptics = LocalHapticFeedback.current

    Column {
        SectionLabel("Saldo")
        Spacer(Modifier.height(Space.xs))

        if (state.booting) {
            Skeleton(width = 180.dp, height = 56.dp)
        } else {
            Figure(account?.balance, style = Xau.type.hero)
        }

        Spacer(Modifier.height(Space.xs))
        Text(
            buildString {
                append(account?.gamertag.orEmpty().ifEmpty { "Nenhuma conta" })
                account?.gamerscore?.takeIf { it > 0 }?.let { append(" · ${money(it)} G") }
            },
            style = Xau.type.secondary,
            color = c.textSecondary,
        )

        // Stale beats wrong: when the last read failed, say so instead of
        // leaving a number that looks current.
        account?.staleReason?.let {
            Spacer(Modifier.height(Space.sm))
            Text(it, style = Xau.type.secondary, color = c.danger)
        }

        // The button claims the *selected* account, so it is gated on
        // that account's pending points — not on the total. Gating on
        // the sum would put a "Resgatar 0 pontos" button on an account
        // whose points are actually waiting somewhere else.
        val mine = account?.unclaimed ?: 0L
        val elsewhere = state.totalWaiting - mine

        AnimatedVisibility(
            visible = mine > 0 && account != null,
            enter = fadeIn() + expandVertically(),
            exit = fadeOut() + shrinkVertically(),
        ) {
            Column {
                Spacer(Modifier.height(Space.lg))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    LiveDot(active = true)
                    Spacer(Modifier.width(Space.sm))
                    Text(
                        "${money(mine)} pontos aguardando resgate",
                        style = Xau.type.secondary,
                        color = c.waiting,
                    )
                }
                Spacer(Modifier.height(Space.md))
                PrimaryButton(
                    label = "Resgatar ${money(mine)} pontos",
                    busy = state.claimingXuid != null,
                    onClick = {
                        haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                        account?.let { onClaim(it.xuid) }
                    },
                )
            }
        }

        // Points pending on an account that is not the one on screen.
        // Said plainly, because the alternative is a user who sees a
        // total they cannot act on and assumes the app is broken.
        AnimatedVisibility(
            visible = elsewhere > 0,
            enter = fadeIn() + expandVertically(),
            exit = fadeOut() + shrinkVertically(),
        ) {
            Column {
                Spacer(Modifier.height(Space.md))
                Text(
                    "${money(elsewhere)} pontos aguardando em outra conta",
                    style = Xau.type.caption,
                    color = c.textMuted,
                )
            }
        }
    }
}

// ------------------------------------------------------------ accounts

@Composable
private fun AccountCard(
    account: Account,
    claiming: Boolean,
    onSelect: () -> Unit,
    onLongPress: () -> Unit,
) {
    val c = Xau.colors
    // The selected account is marked by *value*, not by colour: a
    // brighter fill. Reserving green for the claim button is what keeps
    // the claim button meaning anything.
    val fill = if (account.active) c.surfaceHigh else c.surface

    Box(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.card))
            .background(fill)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.card))
            .pressable(onClick = onSelect, onLongClick = onLongPress)
            .padding(Space.lg),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Avatar(account)
            Spacer(Modifier.width(Space.md))

            Column(Modifier.weight(1f)) {
                Text(
                    account.gamertag.ifEmpty { account.xuid },
                    style = Xau.type.rowTitle,
                    color = c.textPrimary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.height(2.dp))
                Text(
                    account.staleReason ?: "${money(account.gamerscore)} G",
                    style = Xau.type.caption,
                    color = if (account.staleReason != null) c.danger else c.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }

            Spacer(Modifier.width(Space.sm))

            Column(horizontalAlignment = Alignment.End) {
                Figure(account.balance, style = Xau.type.section)
                val waiting = account.unclaimed ?: 0L
                if (waiting > 0) {
                    Spacer(Modifier.height(2.dp))
                    Text(
                        "+${money(waiting)}",
                        style = Xau.type.caption,
                        color = c.waiting,
                    )
                } else if (account.farming) {
                    Spacer(Modifier.height(Space.xs))
                    LiveDot(active = true, modifier = Modifier)
                }
            }
        }

        if (claiming) {
            Text(
                "Resgatando…",
                style = Xau.type.caption,
                color = c.textMuted,
                modifier = Modifier.align(Alignment.BottomEnd),
            )
        }
    }
}

@Composable
private fun Avatar(account: Account) {
    val c = Xau.colors
    Box(
        Modifier
            .size(44.dp)
            .clip(RoundedCornerShape(50))
            .background(c.surfaceHigh),
        contentAlignment = Alignment.Center,
    ) {
        if (account.gamerpic.isNotEmpty()) {
            AsyncImage(
                model = account.gamerpic,
                contentDescription = null,
                modifier = Modifier.fillMaxSize().clip(RoundedCornerShape(50)),
            )
        } else {
            // Xbox does not always return an avatar, and a broken-image
            // glyph in a list of accounts reads as a bug.
            Text(
                account.gamertag.take(1).uppercase(),
                style = Xau.type.rowTitle,
                color = c.textSecondary,
            )
        }
    }
}

@Composable
private fun AccountSkeleton() {
    val c = Xau.colors
    Box(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.card))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.card))
            .padding(Space.lg),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Skeleton(44.dp, 44.dp)
            Spacer(Modifier.width(Space.md))
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(Space.xs)) {
                Skeleton(120.dp, 15.dp)
                Skeleton(70.dp, 11.dp)
            }
            Skeleton(64.dp, 24.dp)
        }
    }
}

@Composable
private fun AddAccountCard(onClick: () -> Unit) {
    val c = Xau.colors
    Box(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.card))
            .border(1.dp, c.outline, RoundedCornerShape(Radius.card))
            .pressable(onClick)
            .padding(Space.lg),
        contentAlignment = Alignment.Center,
    ) {
        Text("Adicionar conta", style = Xau.type.rowTitle, color = c.textSecondary)
    }
}

// ---------------------------------------------------------------- farm

@Composable
private fun FarmCard(farm: FarmState?, onToggle: () -> Unit) {
    val c = Xau.colors
    val running = farm?.running == true
    val haptics = LocalHapticFeedback.current

    Box(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.card))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.card))
            .padding(Space.lg),
    ) {
        Column {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text("Farm de presença", style = Xau.type.rowTitle, color = c.textPrimary)
                    Spacer(Modifier.height(2.dp))
                    Text(
                        "8 títulos de console · ciclo de 16 min",
                        style = Xau.type.caption,
                        color = c.textMuted,
                    )
                }
                Switch(
                    checked = running,
                    onCheckedChange = {
                        haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                        onToggle()
                    },
                    colors = SwitchDefaults.colors(
                        // Green here is a *signal* that the farm is live,
                        // not a second filled action competing with the
                        // claim button — so the track stays dark and only
                        // the thumb carries the colour.
                        checkedThumbColor = c.brandBright,
                        checkedTrackColor = c.surfaceHigh,
                        checkedBorderColor = c.brandBright,
                        uncheckedThumbColor = c.textMuted,
                        uncheckedTrackColor = c.surfaceHigh,
                        uncheckedBorderColor = c.outline,
                    ),
                )
            }

            AnimatedVisibility(visible = running && farm != null) {
                Column {
                    Spacer(Modifier.height(Space.lg))
                    CycleBar(farm)
                    Spacer(Modifier.height(Space.sm))
                    Row(
                        Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                    ) {
                        Text(
                            "Ciclo ${farm?.cycle ?: 0}",
                            style = Xau.type.caption,
                            color = c.textMuted,
                        )
                        Text(
                            "${(farm?.elapsedSecs ?: 0L) / 60} de ${(farm?.totalSecs ?: 960L) / 60} min",
                            style = Xau.type.caption,
                            color = c.textSecondary,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun CycleBar(farm: FarmState?) {
    val c = Xau.colors
    val fraction = ((farm?.percent ?: 0f) / 100f).coerceIn(0f, 1f)
    Box(
        Modifier
            .fillMaxWidth()
            .height(4.dp)
            .clip(RoundedCornerShape(50))
            .background(c.surfaceHigh),
    ) {
        Box(
            Modifier
                .fillMaxWidth(fraction)
                .height(4.dp)
                .clip(RoundedCornerShape(50))
                .background(c.brandBright),
        )
    }
}
