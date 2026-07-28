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
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.xau.rewards.R
import com.xau.rewards.core.AccountOffers
import com.xau.rewards.core.Api
import com.xau.rewards.core.ClaimOffersResult
import com.xau.rewards.core.Offer
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import kotlinx.coroutines.launch

/**
 * Game Pass member offers, across every account.
 *
 * The reward inside an offer — a Discord promo link, a store token — is
 * minted by the claim that consumes it, and only then. So claiming is
 * deliberately unlike the points auto-claim: it never fires on a timer,
 * it asks first, and every reward is written to a CSV on the device
 * before it reaches this screen. A resource that exists only in a toast
 * is a resource that can be lost.
 */
@Composable
fun OffersScreen(onBack: () -> Unit) {
    val c = Xau.colors
    val scope = rememberCoroutineScope()
    val clipboard = LocalClipboardManager.current

    var accounts by remember { mutableStateOf<List<AccountOffers>?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var confirming by remember { mutableStateOf(false) }
    var claiming by remember { mutableStateOf(false) }
    var result by remember { mutableStateOf<ClaimOffersResult?>(null) }

    suspend fun load() {
        runCatching { Api.offersAll() }
            .onSuccess { accounts = it; error = null }
            .onFailure { error = it.message }
    }

    LaunchedEffect(Unit) { load() }

    val available = accounts.orEmpty().sumOf { a -> a.offers.count { it.available } }

    if (confirming) {
        ClaimConfirmDialog(
            count = available,
            onConfirm = {
                confirming = false
                claiming = true
                scope.launch {
                    runCatching { Api.claimOffersAll() }
                        .onSuccess { result = it; load() }
                        .onFailure { error = it.message }
                    claiming = false
                }
            },
            onDismiss = { confirming = false },
        )
    }

    Column(
        Modifier
            .fillMaxSize()
            .background(c.ink)
            .windowInsetsPadding(WindowInsets.safeDrawing),
    ) {
        Header(onBack, available, claiming) { confirming = true }

        when {
            error != null -> StateBlock(
                title = "Não foi possível ler as vantagens",
                detail = error.orEmpty(),
                actionLabel = "Tentar de novo",
                onAction = { scope.launch { load() } },
                tone = c.danger,
            )

            accounts == null -> Column(
                Modifier.fillMaxWidth().padding(Space.gutter),
                verticalArrangement = Arrangement.spacedBy(Space.md),
            ) { repeat(4) { DotLeader(260.dp) } }

            else -> LazyColumn(
                Modifier.fillMaxSize(),
                contentPadding = androidx.compose.foundation.layout.PaddingValues(
                    start = Space.gutter, end = Space.gutter, bottom = Space.xxl,
                ),
                verticalArrangement = Arrangement.spacedBy(Space.sm),
            ) {
                result?.let { r ->
                    item { ResultBanner(r) }
                }
                accounts.orEmpty().forEach { acct ->
                    item { AccountHead(acct) }
                    items(acct.offers.size) { i ->
                        OfferRow(acct.offers[i]) { text ->
                            clipboard.setText(AnnotatedString(text))
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun Header(onBack: () -> Unit, available: Int, claiming: Boolean, onClaim: () -> Unit) {
    val c = Xau.colors
    Column {
        Row(
            Modifier.fillMaxWidth().padding(Space.gutter),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                Modifier.size(Space.touch).clip(RoundedCornerShape(Radius.pill))
                    .clickable(onClick = onBack),
                contentAlignment = Alignment.Center,
            ) { Text("‹", style = Xau.type.hero, color = c.textPrimary) }
            Spacer(Modifier.width(Space.sm))
            Column(Modifier.weight(1f)) {
                Text("Vantagens", style = Xau.type.section, color = c.textPrimary)
                Text(
                    "$available disponíveis em todas as contas",
                    style = Xau.type.caption,
                    color = c.textSecondary,
                )
            }
        }

        if (available > 0) {
            Box(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = Space.gutter)
                    .height(Space.touch)
                    .clip(RoundedCornerShape(Radius.control))
                    .background(if (claiming) c.surfaceHigh else c.greenDeep)
                    .clickable(enabled = !claiming, onClick = onClaim),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    if (claiming) "Resgatando…" else "Resgatar todas ($available)",
                    style = Xau.type.button,
                    color = if (claiming) c.textMuted else c.onGreen,
                )
            }
            Spacer(Modifier.height(Space.md))
        }
    }
}

/**
 * Claiming cannot be undone, so it is confirmed — and the dialog says
 * what actually happens, including where the rewards are written.
 */
@Composable
private fun ClaimConfirmDialog(count: Int, onConfirm: () -> Unit, onDismiss: () -> Unit) {
    val c = Xau.colors
    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = c.surfaceHigh,
        shape = RoundedCornerShape(Radius.card),
        title = { Text("Resgatar $count vantagens?", style = Xau.type.section, color = c.textPrimary) },
        text = {
            Text(
                "O resgate não pode ser desfeito: cada vantagem é consumida na conta " +
                    "onde for resgatada. Os links e códigos recebidos ficam salvos em " +
                    "CSV neste aparelho, um arquivo por vantagem.",
                style = Xau.type.body,
                color = c.textSecondary,
            )
        },
        confirmButton = {
            TextButton(onClick = onConfirm) {
                Text("Resgatar", style = Xau.type.button, color = c.green)
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text("Cancelar", style = Xau.type.button, color = c.textSecondary)
            }
        },
    )
}

@Composable
private fun ResultBanner(r: ClaimOffersResult) {
    val c = Xau.colors
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.row))
            .background(c.surface)
            .border(1.dp, c.green, RoundedCornerShape(Radius.row))
            .padding(Space.lg),
    ) {
        Text(
            "${r.claimed.size} vantagens resgatadas",
            style = Xau.type.rowTitle,
            color = c.green,
        )
        r.indexCsv?.let {
            Spacer(Modifier.height(Space.xs))
            Text("CSV: $it", style = Xau.type.caption, color = c.textMuted, maxLines = 2)
        }
        if (r.failures.isNotEmpty()) {
            Spacer(Modifier.height(Space.sm))
            r.failures.take(3).forEach {
                Text(it, style = Xau.type.caption, color = c.danger, maxLines = 1)
            }
        }
    }
}

@Composable
private fun AccountHead(acct: AccountOffers) {
    val c = Xau.colors
    val avail = acct.offers.count { it.available }
    Row(
        Modifier.fillMaxWidth().padding(top = Space.lg, bottom = Space.xs),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(acct.gamertag, style = Xau.type.rowTitle, color = c.textPrimary)
        Spacer(Modifier.width(Space.sm))
        Text(
            acct.error ?: "$avail disponíveis",
            style = Xau.type.caption,
            color = if (acct.error != null) c.danger else c.textSecondary,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
private fun OfferRow(offer: Offer, onCopy: (String) -> Unit) {
    val c = Xau.colors
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.row))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.row))
            .then(
                // Only a claimed offer has anything to copy.
                offer.resource?.let { Modifier.clickable { onCopy(it) } } ?: Modifier,
            )
            .padding(Space.lg),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (offer.claimed) {
            Icon(
                painterResource(R.drawable.ic_check),
                contentDescription = null,
                tint = c.green,
                modifier = Modifier.size(18.dp),
            )
        } else {
            Box(
                Modifier.size(18.dp).clip(RoundedCornerShape(Radius.pill))
                    .border(1.5.dp, c.green, RoundedCornerShape(Radius.pill)),
            )
        }
        Spacer(Modifier.width(Space.md))

        Column(Modifier.weight(1f)) {
            // The real name, from the content feed. `/v2/offers` carries
            // none, so without it every row read "Código da Store" over a
            // GUID and the user could not tell one perk from another.
            Text(
                offer.title ?: prettyType(offer.resourceType),
                style = Xau.type.rowTitle,
                color = if (offer.claimed) c.textSecondary else c.textPrimary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(2.dp))
            Text(
                offer.resource ?: offer.description ?: prettyType(offer.resourceType),
                style = Xau.type.caption,
                color = if (offer.resource != null) c.green else c.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }

        if (offer.resource != null) {
            Spacer(Modifier.width(Space.sm))
            Text("copiar", style = Xau.type.caption, color = c.textSecondary)
        }
    }
}

private fun prettyType(t: String?): String = when {
    t == null -> "Vantagem"
    t.contains("external-link", true) -> "Link externo"
    t.contains("store-token", true) -> "Código da Store"
    else -> t
}
