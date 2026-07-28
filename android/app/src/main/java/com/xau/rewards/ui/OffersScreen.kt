package com.xau.rewards.ui

import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.xau.rewards.R
import com.xau.rewards.core.AccountOffers
import com.xau.rewards.core.Api
import com.xau.rewards.core.ClaimOffersResult
import com.xau.rewards.core.OfferCode
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Game Pass member offers, across every account.
 *
 * The reward inside an offer — a Discord promo link, a store token — is
 * minted by the claim that consumes it, and only then. So claiming is
 * deliberately unlike the points auto-claim: it never fires on a timer,
 * it asks first, and every reward is written to a CSV on the device
 * before it reaches this screen. A resource that exists only in a toast
 * is a resource that can be lost.
 *
 * The list is one card per reward rather than one per account-and-reward.
 * A reward is the same perk wherever it is claimed; what differs per
 * account is the code it minted, and that belongs in the reward's own
 * detail — where every code for it sits together, ready to be worked
 * through one at a time.
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
    var open by remember { mutableStateOf<OfferGroup?>(null) }

    suspend fun load() {
        runCatching { Api.offersAll() }
            .onSuccess { accounts = it; error = null }
            .onFailure { error = it.message }
    }

    LaunchedEffect(Unit) { load() }

    BackHandler { if (open != null) open = null else onBack() }

    open?.let { group ->
        OfferCodesScreen(group, onBack = { open = null }) {
            clipboard.setText(AnnotatedString(it))
        }
        return
    }

    val groups = remember(accounts) { group(accounts.orEmpty()) }
    val available = groups.sumOf { it.available }
    val failed = accounts.orEmpty().filter { it.error != null }

    if (confirming) {
        ClaimConfirmDialog(
            count = available,
            accounts = accounts.orEmpty().size - failed.size,
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
        Header(
            onBack = onBack,
            available = available,
            accounts = accounts.orEmpty().size - failed.size,
            claiming = claiming,
            onClaim = { confirming = true },
        )

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
                result?.let { r -> item { ResultBanner(r) } }
                // An account whose list failed to load is named rather
                // than silently dropped: its rewards are missing from
                // every count on this screen, and only it knows that.
                failed.forEach { a ->
                    item {
                        Text(
                            "${a.gamertag}: ${a.error}",
                            style = Xau.type.caption,
                            color = c.danger,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.padding(top = Space.sm),
                        )
                    }
                }
                items(groups.size) { i -> OfferCard(groups[i]) { open = groups[i] } }
            }
        }
    }
}

// ------------------------------------------------------------- grouping

/** One reward, and how the accounts stand on it. */
data class OfferGroup(
    val offerId: String,
    val title: String,
    val description: String?,
    val resourceType: String?,
    val accounts: Int,
    val available: Int,
    val claimed: Int,
)

/**
 * Collapse every account's list into one row per reward.
 *
 * Names come from the content feed and one account's response can be
 * missing a title the next one carries, so the first non-null wins
 * rather than whichever account happened to sort first.
 */
private fun group(accounts: List<AccountOffers>): List<OfferGroup> {
    val byId = LinkedHashMap<String, OfferGroup>()
    for (a in accounts) {
        for (o in a.offers) {
            val cur = byId[o.offerId]
            byId[o.offerId] = OfferGroup(
                offerId = o.offerId,
                title = cur?.title?.takeIf { it.isNotBlank() }
                    ?: o.title
                    ?: prettyType(o.resourceType),
                description = cur?.description ?: o.description,
                resourceType = cur?.resourceType ?: o.resourceType,
                accounts = (cur?.accounts ?: 0) + 1,
                available = (cur?.available ?: 0) + if (o.available) 1 else 0,
                claimed = (cur?.claimed ?: 0) + if (o.claimed) 1 else 0,
            )
        }
    }
    // Anything still claimable first — that is what the screen is for.
    return byId.values.sortedWith(
        compareByDescending<OfferGroup> { it.available > 0 }.thenBy { it.title },
    )
}

// --------------------------------------------------------------- pieces

@Composable
private fun Header(
    onBack: () -> Unit,
    available: Int,
    accounts: Int,
    claiming: Boolean,
    onClaim: () -> Unit,
) {
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
                    "$available a resgatar em $accounts ${plural(accounts, "conta", "contas")}",
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
 * what actually happens, including how many accounts it touches and
 * where the rewards are written.
 */
@Composable
private fun ClaimConfirmDialog(
    count: Int,
    accounts: Int,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
) {
    val c = Xau.colors
    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = c.surfaceHigh,
        shape = RoundedCornerShape(Radius.card),
        title = {
            Text("Resgatar $count vantagens?", style = Xau.type.section, color = c.textPrimary)
        },
        text = {
            Text(
                "Vale para ${plural(accounts, "a conta conectada", "as $accounts contas conectadas")}. " +
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
private fun OfferCard(g: OfferGroup, onOpen: () -> Unit) {
    val c = Xau.colors
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.row))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.row))
            .clickable(onClick = onOpen)
            .padding(Space.lg),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (g.available == 0 && g.claimed > 0) {
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
                g.title,
                style = Xau.type.rowTitle,
                color = if (g.available > 0) c.textPrimary else c.textSecondary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(2.dp))
            Text(
                status(g),
                style = Xau.type.caption,
                color = if (g.available > 0) c.green else c.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }

        Spacer(Modifier.width(Space.sm))
        Text("›", style = Xau.type.section, color = c.textMuted)
    }
}

private fun status(g: OfferGroup): String = when {
    g.available > 0 && g.claimed > 0 ->
        "${g.available} a resgatar · ${g.claimed} de ${g.accounts} já resgatadas"
    g.available > 0 -> "${plural(g.available, "1 conta pode", "${g.available} contas podem")} resgatar"
    g.claimed > 0 -> "resgatada em ${plural(g.claimed, "1 conta", "${g.claimed} contas")}"
    else -> "indisponível nas contas conectadas"
}

// --------------------------------------------------------------- detail

/**
 * Every code this reward minted, read back from the CSVs on disk.
 *
 * On disk rather than from the claim that is still in memory: a code
 * claimed on an earlier run — or on an account since signed out — is
 * still the user's, and the files are the record. This is also why
 * claiming writes before it reports.
 */
@Composable
private fun OfferCodesScreen(g: OfferGroup, onBack: () -> Unit, onCopy: (String) -> Unit) {
    val c = Xau.colors
    val scope = rememberCoroutineScope()
    var rows by remember(g.offerId) { mutableStateOf<List<OfferCode>?>(null) }
    var error by remember(g.offerId) { mutableStateOf<String?>(null) }
    // Which row was just copied. Android 13 draws its own clipboard
    // bubble, but nothing below it does, and a tap that appears to do
    // nothing reads as a broken button.
    var copied by remember(g.offerId) { mutableStateOf<Int?>(null) }
    // What came of the last save.
    var saved by remember(g.offerId) { mutableStateOf<String?>(null) }
    val context = LocalContext.current

    LaunchedEffect(g.offerId) {
        runCatching { Api.offerCodes(g.offerId) }
            .onSuccess { rows = it }
            .onFailure { error = it.message }
    }

    // The system's own save dialog. The CSVs live in the app's private
    // directory, which nothing else on the phone can open, so a download
    // is a copy written wherever the user points — and this is the one
    // way to write there that asks for no storage permission at all.
    val save = rememberLauncherForActivityResult(
        ActivityResultContracts.CreateDocument("text/csv"),
    ) { uri ->
        // A cancelled dialog is an answer, not a failure. Say nothing.
        if (uri == null) return@rememberLauncherForActivityResult
        scope.launch {
            runCatching {
                // Read again here, not when the button was tapped: the
                // picker can sit open for minutes, and a claim landing in
                // that window belongs in the file the user is watching
                // themselves save.
                val file = Api.offerCsv(g.offerId, g.title)
                withContext(Dispatchers.IO) {
                    // "wt" truncates. Plain "w" does not on most
                    // providers, so overwriting a longer file would leave
                    // its tail behind and a spreadsheet would show a
                    // phantom extra row of somebody else's codes.
                    context.contentResolver.openOutputStream(uri, "wt")?.use {
                        it.write(file.text.toByteArray(Charsets.UTF_8))
                    } ?: error("o sistema não abriu o arquivo")
                }
            }
                .onSuccess { saved = "CSV salvo" }
                .onFailure { saved = "não deu para salvar: ${it.message}" }
        }
    }

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
                Modifier.size(Space.touch).clip(RoundedCornerShape(Radius.pill))
                    .clickable(onClick = onBack),
                contentAlignment = Alignment.Center,
            ) { Text("‹", style = Xau.type.hero, color = c.textPrimary) }
            Spacer(Modifier.width(Space.sm))
            Column(Modifier.weight(1f)) {
                Text(
                    g.title,
                    style = Xau.type.section,
                    color = c.textPrimary,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    saved
                        ?: rows?.let {
                            plural(it.size, "1 código salvo", "${it.size} códigos salvos")
                        }
                        ?: "lendo o CSV…",
                    style = Xau.type.caption,
                    color = if (saved != null) c.green else c.textSecondary,
                )
            }
            // Only offered once there is something in the file. An empty
            // download is a worse answer than no button.
            if (!rows.isNullOrEmpty()) {
                Spacer(Modifier.width(Space.sm))
                Box(
                    Modifier
                        .height(Space.touch)
                        .clip(RoundedCornerShape(Radius.control))
                        .background(c.surfaceHigh)
                        .clickable {
                            scope.launch {
                                saved = null
                                // Only the name is needed now — the
                                // contents are read when the file is
                                // actually written. `launch` itself can
                                // throw on a ROM with no document
                                // picker, which would take the app down
                                // from inside a click.
                                runCatching {
                                    save.launch(Api.offerCsv(g.offerId, g.title).fileName)
                                }.onFailure {
                                    saved = "este aparelho não tem onde salvar arquivos"
                                }
                            }
                        }
                        .padding(horizontal = Space.lg),
                    contentAlignment = Alignment.Center,
                ) { Text("Baixar CSV", style = Xau.type.button, color = c.textPrimary) }
            }
        }

        when {
            error != null -> StateBlock(
                title = "Não foi possível ler o CSV",
                detail = error.orEmpty(),
                tone = c.danger,
            )

            rows == null -> Column(
                Modifier.fillMaxWidth().padding(Space.gutter),
                verticalArrangement = Arrangement.spacedBy(Space.md),
            ) { repeat(3) { DotLeader(220.dp) } }

            rows.orEmpty().isEmpty() -> StateBlock(
                title = "Nenhum código ainda",
                detail = "Um código só existe depois do resgate — é o resgate que o cria. " +
                    "Resgates feitos fora do app não aparecem aqui.",
            )

            else -> LazyColumn(
                Modifier.fillMaxSize(),
                contentPadding = androidx.compose.foundation.layout.PaddingValues(
                    start = Space.gutter, end = Space.gutter, bottom = Space.xxl,
                ),
                verticalArrangement = Arrangement.spacedBy(Space.sm),
            ) {
                item { CodesHeader() }
                items(rows.orEmpty().size) { i ->
                    val r = rows.orEmpty()[i]
                    CodeRow(r, copied == i) {
                        onCopy(r.code)
                        copied = i
                        scope.launch {
                            kotlinx.coroutines.delay(1_600)
                            if (copied == i) copied = null
                        }
                    }
                }
            }
        }
    }
}

/** The two columns of the CSV, named once above the rows. */
@Composable
private fun CodesHeader() {
    val c = Xau.colors
    Row(Modifier.fillMaxWidth().padding(top = Space.xs, bottom = Space.xs)) {
        Text("CÓDIGO / LINK", style = Xau.type.captionStrong, color = c.textMuted)
        Spacer(Modifier.weight(1f))
        Text("DATA", style = Xau.type.captionStrong, color = c.textMuted)
    }
}

@Composable
private fun CodeRow(r: OfferCode, justCopied: Boolean, onCopy: () -> Unit) {
    val c = Xau.colors
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radius.row))
            .background(c.surface)
            .border(1.dp, c.outline, RoundedCornerShape(Radius.row))
            .clickable(onClick = onCopy)
            .padding(Space.lg),
    ) {
        // Three lines, not one truncated one: a promo URL is the thing
        // the user came for, and a link cut off at the ellipsis cannot be
        // read back to check it is the right one.
        Text(
            r.code.ifBlank { "—" },
            style = Xau.type.body,
            color = if (r.code.isBlank()) c.textMuted else c.green,
            maxLines = 3,
            overflow = TextOverflow.Ellipsis,
        )
        Spacer(Modifier.height(Space.sm))
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(prettyDate(r.claimedDate), style = Xau.type.caption, color = c.textSecondary)
            Spacer(Modifier.weight(1f))
            Text(
                if (justCopied) "copiado" else "copiar",
                style = if (justCopied) Xau.type.captionStrong else Xau.type.caption,
                color = if (justCopied) c.green else c.textMuted,
            )
        }
    }
}

// --------------------------------------------------------------- format

/**
 * `2026-07-28T18:24:59Z` as `28/07/2026 · 15:24` — in the phone's zone.
 *
 * The service writes UTC and the CSV keeps it that way, which is right
 * for a file. On screen it is wrong: a code claimed at 15:24 reading
 * 18:24 makes the user doubt it is theirs. Converted here, at the last
 * possible moment, so the record on disk stays unambiguous.
 *
 * Anything that does not parse is shown exactly as written, rather than
 * throwing inside a list item over a date.
 */
private fun prettyDate(iso: String): String {
    if (iso.isBlank()) return "—"
    return runCatching {
        java.time.Instant.parse(iso)
            .atZone(java.time.ZoneId.systemDefault())
            .format(java.time.format.DateTimeFormatter.ofPattern("dd/MM/yyyy · HH:mm"))
    }.getOrDefault(iso)
}

private fun plural(n: Int, one: String, many: String): String = if (n == 1) one else many

private fun prettyType(t: String?): String = when {
    t == null -> "Vantagem"
    t.contains("external-link", true) -> "Link externo"
    t.contains("store-token", true) -> "Código da Store"
    else -> t
}
