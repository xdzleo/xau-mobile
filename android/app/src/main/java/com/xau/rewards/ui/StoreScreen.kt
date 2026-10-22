package com.xau.rewards.ui

import android.annotation.SuppressLint
import android.util.Log
import android.webkit.CookieManager
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
import androidx.compose.foundation.layout.wrapContentHeight
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import coil.compose.AsyncImage
import com.xau.rewards.core.Account
import com.xau.rewards.core.Api
import com.xau.rewards.core.PurchaseForm
import com.xau.rewards.core.StoreProduct
import com.xau.rewards.ui.theme.Radius
import org.json.JSONObject
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
    val ctx = LocalContext.current

    // Starred games: kept on the phone, so every profile sees the same.
    val favorites = remember { mutableStateListOf<StoreProduct>().apply { addAll(StoreFavorites.load(ctx)) } }
    val toggleFavorite = { p: StoreProduct ->
        val at = favorites.indexOfFirst { it.productId == p.productId }
        if (at >= 0) favorites.removeAt(at) else favorites.add(0, p)
        StoreFavorites.save(ctx, favorites.toList())
    }
    var filter by remember { mutableStateOf(StoreFilter()) }

    var query by remember { mutableStateOf("") }
    var shownQuery by remember { mutableStateOf("") }
    val products = remember { mutableStateListOf<StoreProduct>() }
    var cursor by remember { mutableStateOf<String?>(null) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf<String?>(null) }
    var open by remember { mutableStateOf<StoreProduct?>(null) }
    var buying by remember { mutableStateOf<Pair<PurchaseForm, String>?>(null) }
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

    // What the grid shows: games come back already filtered from the
    // native side, which reads, filters and caches the Store's pages.
    var shownFilter by remember { mutableStateOf(StoreFilter()) }
    val seen = remember { HashSet<String>() }
    suspend fun browse(q: String, f: StoreFilter, next: String?) =
        Api.storeBrowse(q, f.wantsPaid, next, f.microsoftOnly, f.hideFree, f.min, f.max)

    // Typing waits for a pause; a chip answers at once (its pages are
    // usually cached already); an empty box shows what is trending.
    LaunchedEffect(query, filter) {
        val q = query.trim()
        if (q.isNotEmpty() && q.length < 2) return@LaunchedEffect
        val typed = q != shownQuery || filter.minPrice != shownFilter.minPrice || filter.maxPrice != shownFilter.maxPrice
        if (typed && (q.isNotEmpty() || filter.min != null || filter.max != null)) delay(300)
        loading = true
        error = null
        runCatching { browse(q, filter, null) }
            .onSuccess { page ->
                products.clear()
                seen.clear()
                page.products.forEach { if (seen.add(it.productId)) products.add(it) }
                cursor = page.cursor
                shownQuery = q
                shownFilter = filter
                grid.scrollToItem(0)
            }
            .onFailure { error = it.message }
        loading = false
    }

    // The next page, when the end of the grid comes into view.
    val nearEnd by remember {
        derivedStateOf {
            val last = grid.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0
            last >= grid.layoutInfo.totalItemsCount - 6
        }
    }
    val visible = products
    // A narrow filter can come back with less than a screen, nothing to
    // scroll: keep asking until the screen fills or the list runs out.
    val needMore = nearEnd || products.size < 18
    // (Not keyed on `loading`: setting it here would cancel this very load.)
    LaunchedEffect(needMore, cursor, products.size) {
        val next = cursor
        if (needMore && next != null && !loading) {
            loading = true
            val forQuery = shownQuery
            val forFilter = shownFilter
            runCatching { browse(forQuery, forFilter, next) }
                .onSuccess { page ->
                    // A newer search or filter replaced the list meanwhile.
                    if (forQuery == shownQuery && forFilter == shownFilter) {
                        page.products.forEach { if (seen.add(it.productId)) products.add(it) }
                        cursor = page.cursor
                    }
                }
                .onFailure { error = it.message }
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

    buying?.let { (form, title) ->
        PurchasePage(form = form, title = title, onClose = { buying = null })
        return
    }

    Box(Modifier.fillMaxSize().background(c.ink)) {
        LazyVerticalGrid(
            columns = GridCells.Fixed(3),
            state = grid,
            modifier = Modifier.fillMaxSize().hazeSource(haze),
            contentPadding = PaddingValues(
                start = Space.gutter,
                end = Space.gutter,
                top = headerHeight,
                bottom = Space.huge + navBar,
            ),
            horizontalArrangement = Arrangement.spacedBy(Space.sm),
            verticalArrangement = Arrangement.spacedBy(Space.md),
        ) {
            // Favorites open the store, whoever is signed in.
            if (favorites.isNotEmpty() && shownQuery.isEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Column {
                        Text(
                            "♥ Favoritos",
                            style = Xau.type.section,
                            color = c.textPrimary,
                            modifier = Modifier.padding(top = Space.sm, bottom = Space.sm),
                        )
                        LazyRow(horizontalArrangement = Arrangement.spacedBy(Space.sm)) {
                            items(favorites.toList(), key = { it.productId }) { p ->
                                Box(Modifier.width(104.dp)) {
                                    ProductCard(p, favorite = true, onFavorite = { toggleFavorite(p) }) { open = p }
                                }
                            }
                        }
                    }
                }
            }
            item(span = { GridItemSpan(maxLineSpan) }) {
                Text(
                    if (shownQuery.isEmpty()) "Em alta na Microsoft Store" else "Resultados para “$shownQuery”",
                    style = Xau.type.section,
                    color = c.textPrimary,
                    modifier = Modifier.padding(top = Space.sm),
                )
            }
            if (error != null && products.isEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Text(error.orEmpty(), style = Xau.type.body, color = c.danger)
                }
            }
            if (filter.active && !loading && visible.isEmpty() && products.isNotEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Text(
                        "Nenhum jogo desta lista passa pelos filtros.",
                        style = Xau.type.body,
                        color = c.textMuted,
                    )
                }
            }
            items(visible.size, key = { visible[it].productId }) { i ->
                val p = visible[i]
                Box(Modifier.entrance(i % 6)) {
                    ProductCard(
                        p,
                        favorite = favorites.any { it.productId == p.productId },
                        onFavorite = { toggleFavorite(p) },
                    ) { open = p }
                }
            }
            if (loading) {
                items(if (products.isEmpty()) 9 else 3) {
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
            FilterRow(filter) { filter = it }
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
                ProductSheet(
                    p,
                    favorite = favorites.any { it.productId == p.productId },
                    onFavorite = { toggleFavorite(p) },
                ) {
                    scope.launch {
                        // Signed in like the Xbox app; without a Windows
                        // sign-in, the public product page.
                        runCatching { Api.storePurchaseForm(p.productId) }
                            .onFailure { Log.w("XauStore", "checkout sem login: ${it.message}") }
                            .recoverCatching { PurchaseForm(Api.storePurchaseUrl(p.productId), "", emptyList()) }
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
private fun ProductCard(p: StoreProduct, favorite: Boolean, onFavorite: () -> Unit, onClick: () -> Unit) {
    val c = Xau.colors
    Column(Modifier.fillMaxWidth().pressable(onClick)) {
        Box(
            Modifier
                .fillMaxWidth()
                .aspectRatio(0.72f)
                .clip(RoundedCornerShape(Radius.control))
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
                    style = Xau.type.captionStrong.copy(fontSize = 8.sp),
                    color = c.ink,
                    modifier = Modifier
                        .padding(4.dp)
                        .clip(RoundedCornerShape(Radius.pill))
                        .background(Brush.horizontalGradient(listOf(Color(0xFF9BF00B), c.green)))
                        .padding(horizontal = 5.dp, vertical = 1.dp),
                )
            }
            HeartButton(
                favorite,
                onFavorite,
                Modifier.align(Alignment.BottomEnd).padding(4.dp).size(28.dp),
            )
        }
        Spacer(Modifier.height(Space.xs))
        Text(p.title, style = Xau.type.rowTitle.copy(fontSize = 12.sp), color = c.textPrimary, maxLines = 2, overflow = TextOverflow.Ellipsis)
        PriceLine(p)
    }
}

@Composable
private fun HeartButton(on: Boolean, onToggle: () -> Unit, modifier: Modifier = Modifier) {
    val c = Xau.colors
    Box(
        modifier
            .clip(RoundedCornerShape(Radius.pill))
            .background(Color.Black.copy(alpha = 0.5f))
            .clickable(onClick = onToggle),
        contentAlignment = Alignment.Center,
    ) {
        Text(if (on) "♥" else "♡", fontSize = 16.sp, color = if (on) Color(0xFFFF4D6D) else c.textPrimary)
    }
}

/** Only games Microsoft publishes, and a price range in reais. */
@Composable
private fun FilterRow(filter: StoreFilter, onChange: (StoreFilter) -> Unit) {
    val c = Xau.colors
    Row(
        Modifier.fillMaxWidth().padding(top = Space.xs),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(Space.sm),
    ) {
        FilterChip("Microsoft", filter.microsoftOnly) { onChange(filter.copy(microsoftOnly = !filter.microsoftOnly)) }
        FilterChip("Sem grátis", filter.hideFree) { onChange(filter.copy(hideFree = !filter.hideFree)) }
    }
    Row(
        Modifier.fillMaxWidth().padding(top = Space.xs),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(Space.sm),
    ) {
        MoneyField("Mín R$", filter.minPrice, { onChange(filter.copy(minPrice = it)) }, Modifier.weight(1f))
        MoneyField("Máx R$", filter.maxPrice, { onChange(filter.copy(maxPrice = it)) }, Modifier.weight(1f))
        if (filter.active) {
            Text(
                "✕",
                style = Xau.type.captionStrong,
                color = c.textSecondary,
                modifier = Modifier
                    .size(36.dp)
                    .clip(RoundedCornerShape(Radius.pill))
                    .clickable { onChange(StoreFilter()) }
                    .wrapContentHeight(Alignment.CenterVertically)
                    .align(Alignment.CenterVertically),
                textAlign = androidx.compose.ui.text.style.TextAlign.Center,
            )
        }
    }
}

@Composable
private fun FilterChip(label: String, on: Boolean, onClick: () -> Unit) {
    val c = Xau.colors
    Text(
        label,
        style = Xau.type.captionStrong,
        color = if (on) c.ink else c.textPrimary,
        modifier = Modifier
            .height(36.dp)
            .clip(RoundedCornerShape(Radius.pill))
            .background(if (on) c.green else Color.White.copy(alpha = 0.08f))
            .border(1.dp, Color.White.copy(alpha = 0.10f), RoundedCornerShape(Radius.pill))
            .clickable(onClick = onClick)
            .padding(horizontal = Space.md)
            .wrapContentHeight(Alignment.CenterVertically),
    )
}

@Composable
private fun MoneyField(hint: String, value: String, onChange: (String) -> Unit, modifier: Modifier = Modifier) {
    val c = Xau.colors
    Box(
        modifier
            .height(36.dp)
            .clip(RoundedCornerShape(Radius.pill))
            .background(Color.White.copy(alpha = 0.08f))
            .border(1.dp, Color.White.copy(alpha = 0.10f), RoundedCornerShape(Radius.pill))
            .padding(horizontal = Space.md),
        contentAlignment = Alignment.CenterStart,
    ) {
        if (value.isEmpty()) Text(hint, style = Xau.type.caption, color = c.textMuted)
        BasicTextField(
            value = value,
            onValueChange = { v -> onChange(v.filter { it.isDigit() || it == ',' || it == '.' }.take(8)) },
            singleLine = true,
            textStyle = Xau.type.caption.copy(color = c.textPrimary),
            cursorBrush = SolidColor(c.green),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Decimal),
            modifier = Modifier.fillMaxWidth(),
        )
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
private fun ProductSheet(p: StoreProduct, favorite: Boolean, onFavorite: () -> Unit, onBuy: () -> Unit) {
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
        Row(verticalAlignment = Alignment.CenterVertically) {
            HeartButton(favorite, onFavorite, Modifier.size(Space.touch))
            Spacer(Modifier.width(Space.md))
            Box(Modifier.weight(1f)) { PrimaryButton(label = "Comprar na Microsoft Store", onClick = onBuy) }
        }
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
private fun PurchasePage(form: PurchaseForm, title: String, onClose: () -> Unit) {
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
                    // The Store checks the Microsoft session through
                    // login.live.com in an iframe; without third-party
                    // cookies that check fails and it asks to sign in again.
                    CookieManager.getInstance().setAcceptCookie(true)
                    CookieManager.getInstance().setAcceptThirdPartyCookies(this, true)
                    // The checkout refuses a form posted from no origin, so
                    // it is posted from a page on its own host.
                    var posted = form.action.isEmpty()
                    webViewClient = object : WebViewClient() {
                        override fun onPageFinished(v: WebView?, pageUrl: String?) {
                            if (posted || pageUrl?.startsWith(form.host) != true) return
                            posted = true
                            v?.evaluateJavascript(submitScript(form), null)
                        }
                    }
                    loadUrl(form.host)
                    web = this
                }
            },
        )
    }
}

/** Build and submit [form] in the page, the token staying in the body. */
private fun submitScript(form: PurchaseForm): String = buildString {
    append("(function(){var f=document.createElement('form');f.method='POST';f.action=")
    append(JSONObject.quote(form.action)).append(';')
    for ((name, value) in form.fields) {
        append("var i=document.createElement('input');i.type='hidden';i.name=")
        append(JSONObject.quote(name)).append(";i.value=").append(JSONObject.quote(value))
        append(";f.appendChild(i);")
    }
    append("document.documentElement.appendChild(f);f.submit();})()")
}
