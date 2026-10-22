package com.xau.rewards.ui

import android.annotation.SuppressLint
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.rememberLazyGridState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import coil.compose.AsyncImage
import com.xau.rewards.core.Account
import com.xau.rewards.core.Api
import com.xau.rewards.core.StoreProduct
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.HazeStyle
import dev.chrisbanes.haze.HazeTint
import dev.chrisbanes.haze.hazeEffect
import dev.chrisbanes.haze.hazeSource
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * The Xbox store, opened from an account.
 *
 * The catalogue is the Microsoft Store's own (search and what is
 * trending, the endpoints the xodus-nexus launcher uses). The games
 * scroll under a glass header — a real backdrop blur — that carries the
 * search and a liquid-glass banner with this account's Rewards balance,
 * re-read every 15 seconds while the store is open.
 *
 * Buying happens on the Store's own product page, in the app, where the
 * user signs in, confirms and pays exactly as on the web.
 */
@Composable
fun StoreScreen(
    account: Account,
    onBack: () -> Unit,
    onOpenMissions: () -> Unit,
    onRefresh: () -> Unit,
) {
    val c = Xau.colors
    val scope = rememberCoroutineScope()
    val haze = remember { HazeState() }
    val grid = rememberLazyGridState()
    val focus = LocalFocusManager.current

    var query by remember { mutableStateOf("") }
    var shownQuery by remember { mutableStateOf("") }
    val products = remember { mutableStateListOf<StoreProduct>() }
    var cursor by remember { mutableStateOf<String?>(null) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf<String?>(null) }
    var open by remember { mutableStateOf<StoreProduct?>(null) }
    var buying by remember { mutableStateOf<Pair<String, String>?>(null) }
    // The glass header is measured, not guessed: the grid starts right
    // under it whatever the font scale or status-bar height.
    val density = LocalDensity.current
    var headerHeight by remember { mutableStateOf(200.dp) }
    val navBar = WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding()

    // Real time: the balance is re-read while the store is in front.
    LaunchedEffect(account.xuid) {
        while (true) {
            onRefresh()
            delay(15_000)
        }
    }

    // Typing searches by itself, after a pause; an empty box shows what
    // is trending.
    LaunchedEffect(query) {
        if (query.isNotBlank() && query.trim().length < 2) return@LaunchedEffect
        if (query.isNotBlank()) delay(450)
        loading = true
        error = null
        runCatching { if (query.isBlank()) Api.storeFeatured() else Api.storeSearch(query) }
            .onSuccess {
                products.clear()
                products.addAll(it.products)
                cursor = it.cursor
                shownQuery = query.trim()
                grid.scrollToItem(0)
            }
            .onFailure { error = it.message }
        loading = false
    }

    // The next page, when the end of the grid comes into view.
    val nearEnd by remember {
        derivedStateOf {
            val last = grid.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0
            last >= grid.layoutInfo.totalItemsCount - 4
        }
    }
    LaunchedEffect(nearEnd, cursor) {
        val next = cursor
        if (nearEnd && next != null && !loading && shownQuery.isNotEmpty()) {
            loading = true
            runCatching { Api.storeSearch(shownQuery, next) }
                .onSuccess {
                    products.addAll(it.products.filter { p -> products.none { it.productId == p.productId } })
                    cursor = it.cursor
                }
            loading = false
        }
    }

    val back = {
        when {
            buying != null -> buying = null
            open != null -> open = null
            else -> onBack()
        }
    }
    BackHandler(onBack = back)

    buying?.let { (url, title) ->
        PurchasePage(url = url, title = title, onClose = { buying = null })
        return
    }

    Box(Modifier.fillMaxSize().background(c.ink)) {
        LazyVerticalGrid(
            columns = GridCells.Fixed(2),
            state = grid,
            modifier = Modifier.fillMaxSize().hazeSource(haze),
            contentPadding = PaddingValues(
                start = Space.gutter,
                end = Space.gutter,
                top = headerHeight,
                bottom = Space.huge + navBar,
            ),
            horizontalArrangement = Arrangement.spacedBy(Space.md),
            verticalArrangement = Arrangement.spacedBy(Space.lg),
        ) {
            item(span = { GridItemSpan(2) }) {
                Text(
                    if (shownQuery.isEmpty()) "Em alta na Microsoft Store" else "Resultados para “$shownQuery”",
                    style = Xau.type.section,
                    color = c.textPrimary,
                    modifier = Modifier.padding(top = Space.sm),
                )
            }
            if (error != null && products.isEmpty()) {
                item(span = { GridItemSpan(2) }) {
                    Text(error.orEmpty(), style = Xau.type.body, color = c.danger)
                }
            }
            items(products.size, key = { products[it].productId }) { i ->
                Box(Modifier.entrance(i % 6)) {
                    ProductCard(products[i]) { open = products[i] }
                }
            }
            if (loading) {
                items(if (products.isEmpty()) 6 else 2) {
                    Box(
                        Modifier
                            .fillMaxWidth()
                            .aspectRatio(0.72f)
                            .clip(RoundedCornerShape(Radius.card))
                            .background(c.surface),
                    )
                }
            }
        }

        // The glass header: search and the live Rewards banner, over the
        // games, blurring what scrolls beneath.
        Column(
            Modifier
                .fillMaxWidth()
                .hazeEffect(
                    haze,
                    HazeStyle(
                        backgroundColor = c.ink,
                        tint = HazeTint(c.ink.copy(alpha = 0.55f)),
                        blurRadius = 28.dp,
                        noiseFactor = 0.06f,
                    ),
                )
                .onSizeChanged { headerHeight = with(density) { it.height.toDp() } }
                .windowInsetsPadding(WindowInsets.safeDrawing)
                .padding(horizontal = Space.gutter)
                .padding(bottom = Space.md),
        ) {
            Row(Modifier.fillMaxWidth().padding(vertical = Space.sm), verticalAlignment = Alignment.CenterVertically) {
                Box(
                    Modifier.size(Space.touch).clip(RoundedCornerShape(Radius.pill)).clickable { back() },
                    contentAlignment = Alignment.Center,
                ) { Text("‹", style = Xau.type.hero.copy(fontSize = 34.sp), color = c.textPrimary) }
                Spacer(Modifier.width(Space.xs))
                SearchField(
                    value = query,
                    onChange = { query = it },
                    onSubmit = { focus.clearFocus() },
                    modifier = Modifier.weight(1f),
                )
            }
            Box(Modifier.entrance(0, rise = 20.dp)) {
                GlassBanner(account, onClick = onOpenMissions)
            }
        }

        // Product detail, sliding up over the grid.
        AnimatedVisibility(
            visible = open != null,
            enter = fadeIn(tween(180)),
            exit = fadeOut(tween(180)),
        ) {
            Box(
                Modifier
                    .fillMaxSize()
                    .background(Color.Black.copy(alpha = 0.6f))
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                    ) { open = null },
            )
        }
        AnimatedVisibility(
            visible = open != null,
            modifier = Modifier.align(Alignment.BottomCenter),
            enter = slideInVertically(tween(320)) { it } + fadeIn(),
            exit = slideOutVertically(tween(220)) { it } + fadeOut(),
        ) {
            open?.let { p ->
                ProductSheet(p) {
                    scope.launch {
                        runCatching { Api.storePurchaseUrl(p.productId) }
                            .onSuccess { buying = it to p.title }
                    }
                }
            }
        }
    }
}

@Composable
private fun SearchField(
    value: String,
    onChange: (String) -> Unit,
    onSubmit: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val c = Xau.colors
    Box(
        modifier
            .height(Space.touch)
            .clip(RoundedCornerShape(Radius.pill))
            .background(Color.White.copy(alpha = 0.08f))
            .border(1.dp, Color.White.copy(alpha = 0.10f), RoundedCornerShape(Radius.pill))
            .padding(horizontal = Space.lg),
        contentAlignment = Alignment.CenterStart,
    ) {
        if (value.isEmpty()) {
            Text("Buscar jogos na loja", style = Xau.type.body, color = c.textMuted)
        }
        BasicTextField(
            value = value,
            onValueChange = onChange,
            singleLine = true,
            textStyle = Xau.type.body.copy(color = c.textPrimary),
            cursorBrush = SolidColor(c.green),
            keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
            keyboardActions = KeyboardActions(onSearch = { onSubmit() }),
            modifier = Modifier.fillMaxWidth(),
        )
    }
}

/**
 * Liquid glass: a translucent pane over the blurred games, a bright rim
 * that fades round its edge, a specular highlight that slowly sweeps
 * across it, and the account's Rewards balance counting to each new
 * reading.
 */
@Composable
private fun GlassBanner(account: Account, onClick: () -> Unit) {
    val c = Xau.colors
    val shape = RoundedCornerShape(Radius.card + 6.dp)
    val sweep = rememberInfiniteTransition(label = "glass")
    val shine by sweep.animateFloat(
        initialValue = -0.4f,
        targetValue = 1.4f,
        animationSpec = infiniteRepeatable(tween(3_600, easing = LinearEasing), RepeatMode.Restart),
        label = "shine",
    )
    Row(
        Modifier
            .fillMaxWidth()
            .padding(top = Space.sm)
            .clip(shape)
            .background(
                Brush.linearGradient(
                    listOf(
                        Color.White.copy(alpha = 0.16f),
                        c.green.copy(alpha = 0.10f),
                        Color.White.copy(alpha = 0.05f),
                    ),
                ),
            )
            .border(
                1.dp,
                Brush.linearGradient(
                    listOf(Color.White.copy(alpha = 0.55f), Color.White.copy(alpha = 0.06f), c.green.copy(alpha = 0.45f)),
                ),
                shape,
            )
            // The sweep is drawn, not composed: it costs a redraw, never a
            // recomposition, so it stays smooth at 120 Hz.
            .drawWithContent {
                drawContent()
                val x = size.width * shine
                drawRect(
                    Brush.linearGradient(
                        listOf(Color.Transparent, Color.White.copy(alpha = 0.10f), Color.Transparent),
                        start = Offset(x - size.width * 0.25f, 0f),
                        end = Offset(x + size.width * 0.25f, size.height),
                    ),
                )
            }
            .clickable(onClick = onClick)
            .padding(horizontal = Space.lg, vertical = Space.md),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(46.dp).clip(RoundedCornerShape(Radius.pill)).background(c.surfaceHigh)) {
            if (account.gamerpic.isNotEmpty()) {
                AsyncImage(
                    model = account.gamerpic,
                    contentDescription = null,
                    contentScale = ContentScale.Crop,
                    modifier = Modifier.fillMaxSize(),
                )
            }
        }
        Spacer(Modifier.width(Space.md))
        Column(Modifier.weight(1f)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                LiveDot(active = account.balance != null)
                Spacer(Modifier.width(Space.xs + 2.dp))
                Text("REWARDS · AO VIVO", style = Xau.type.captionStrong, color = c.green)
            }
            Text(
                money(countUp(account.balance)),
                style = Xau.type.hero.copy(fontSize = 30.sp),
                color = c.textPrimary,
                maxLines = 1,
            )
            Text(
                "${account.gamertag} · Nível ${account.level ?: "—"}",
                style = Xau.type.caption,
                color = c.textSecondary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Column(horizontalAlignment = Alignment.End) {
            Text("Missões", style = Xau.type.caption, color = c.textSecondary)
            Text("${account.missionsOpen ?: "—"} ›", style = Xau.type.rowFigure, color = c.textPrimary)
        }
    }
}

@Composable
private fun ProductCard(p: StoreProduct, onClick: () -> Unit) {
    val c = Xau.colors
    Column(Modifier.fillMaxWidth().pressable(onClick)) {
        Box(
            Modifier
                .fillMaxWidth()
                .aspectRatio(0.72f)
                .clip(RoundedCornerShape(Radius.card))
                .background(c.surfaceHigh),
        ) {
            if (p.image != null) {
                AsyncImage(
                    model = p.image,
                    contentDescription = p.title,
                    contentScale = ContentScale.Crop,
                    modifier = Modifier.fillMaxSize(),
                )
            }
            if (p.pass != null) {
                Text(
                    "GAME PASS",
                    style = Xau.type.captionStrong.copy(fontSize = 10.sp),
                    color = c.ink,
                    modifier = Modifier
                        .padding(Space.sm)
                        .clip(RoundedCornerShape(Radius.pill))
                        .background(Brush.horizontalGradient(listOf(Color(0xFF9BF00B), c.green)))
                        .padding(horizontal = Space.sm, vertical = 2.dp),
                )
            }
        }
        Spacer(Modifier.height(Space.sm))
        Text(p.title, style = Xau.type.rowTitle.copy(fontSize = 14.sp), color = c.textPrimary, maxLines = 2, overflow = TextOverflow.Ellipsis)
        PriceLine(p)
    }
}

@Composable
private fun PriceLine(p: StoreProduct) {
    val c = Xau.colors
    Row(verticalAlignment = Alignment.CenterVertically) {
        p.was?.let {
            Text(it, style = Xau.type.caption.copy(textDecoration = TextDecoration.LineThrough), color = c.textMuted)
            Spacer(Modifier.width(Space.xs))
        }
        Text(p.price ?: "—", style = Xau.type.captionStrong, color = if (p.was != null) c.green else c.textSecondary)
    }
}

/** The product, and the way to buy it. */
@Composable
private fun ProductSheet(p: StoreProduct, onBuy: () -> Unit) {
    val c = Xau.colors
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(topStart = Radius.card + 8.dp, topEnd = Radius.card + 8.dp))
            .background(c.surface)
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {}
            .windowInsetsPadding(WindowInsets.navigationBars)
            .padding(Space.xl),
    ) {
        Box(
            Modifier
                .align(Alignment.CenterHorizontally)
                .size(width = 40.dp, height = 4.dp)
                .clip(RoundedCornerShape(Radius.pill))
                .background(c.outline),
        )
        Spacer(Modifier.height(Space.lg))
        Row {
            Box(
                Modifier
                    .width(110.dp)
                    .aspectRatio(0.72f)
                    .clip(RoundedCornerShape(Radius.control))
                    .background(c.surfaceHigh),
            ) {
                if (p.image != null) {
                    AsyncImage(p.image, p.title, contentScale = ContentScale.Crop, modifier = Modifier.fillMaxSize())
                }
            }
            Spacer(Modifier.width(Space.lg))
            Column(Modifier.weight(1f)) {
                Text(p.title, style = Xau.type.section, color = c.textPrimary)
                p.publisher?.let { Text(it, style = Xau.type.caption, color = c.textSecondary) }
                Spacer(Modifier.height(Space.sm))
                p.rating?.let { Text("★ %.1f".format(it), style = Xau.type.captionStrong, color = c.textPrimary) }
                Spacer(Modifier.height(Space.sm))
                PriceLine(p)
                p.pass?.let {
                    Spacer(Modifier.height(Space.xs))
                    Text("Incluso no $it", style = Xau.type.captionStrong, color = c.green)
                }
            }
        }
        Spacer(Modifier.height(Space.xl))
        PrimaryButton(label = "Comprar na Microsoft Store", onClick = onBuy)
        Spacer(Modifier.height(Space.sm))
        Text(
            "Abre a página oficial do jogo aqui no app. Você confirma e paga lá, com a sua conta Microsoft.",
            style = Xau.type.caption,
            color = c.textMuted,
        )
    }
}

/** The Store's product page, in the app, where the purchase is made. */
@SuppressLint("SetJavaScriptEnabled")
@Composable
private fun PurchasePage(url: String, title: String, onClose: () -> Unit) {
    val c = Xau.colors
    var web by remember { mutableStateOf<WebView?>(null) }
    BackHandler {
        val w = web
        if (w != null && w.canGoBack()) w.goBack() else onClose()
    }
    Column(Modifier.fillMaxSize().background(c.ink).windowInsetsPadding(WindowInsets.safeDrawing)) {
        Row(Modifier.fillMaxWidth().padding(Space.sm), verticalAlignment = Alignment.CenterVertically) {
            Box(
                Modifier.size(Space.touch).clip(RoundedCornerShape(Radius.pill)).clickable(onClick = onClose),
                contentAlignment = Alignment.Center,
            ) { Text("✕", style = Xau.type.section, color = c.textPrimary) }
            Spacer(Modifier.width(Space.sm))
            Text(title, style = Xau.type.rowTitle, color = c.textPrimary, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                WebView(ctx).apply {
                    settings.javaScriptEnabled = true
                    settings.domStorageEnabled = true
                    webViewClient = WebViewClient()
                    loadUrl(url)
                    web = this
                }
            },
        )
    }
}
