package com.xau.rewards.ui

import android.annotation.SuppressLint
import android.webkit.CookieManager
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.compose.foundation.background
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
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import com.xau.rewards.R
import com.xau.rewards.core.Api
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import kotlinx.coroutines.launch

/**
 * Sign-in, in a WebView.
 *
 * This is the whole reason the app needs no Xbox app installed: the user
 * completes Microsoft's own OAuth page here, and the authorization code
 * comes back on the redirect.
 *
 * **Every navigation is classified by Rust**, never by matching on
 * `?code=` here. Only the exact redirect endpoint is terminal — treating
 * any URL carrying a `code` parameter as success would let a page
 * reached mid-flow hand over a code to exchange.
 */
@SuppressLint("SetJavaScriptEnabled")
@Composable
fun LoginScreen(
    onCode: (String) -> Unit,
    onCancel: () -> Unit,
    onError: (String) -> Unit,
) {
    val c = Xau.colors
    // The WebView is a platform View, so it needs a plain ARGB int
    // rather than a Compose Color.
    val ink = c.ink.toArgb()
    val scope = rememberCoroutineScope()
    var url by remember { mutableStateOf<String?>(null) }
    var failure by remember { mutableStateOf<String?>(null) }
    // Guards against a redirect being classified twice while the first
    // exchange is still in flight — an authorization code is single-use,
    // and the second attempt would fail on a code that already worked.
    var consumed by remember { mutableStateOf(false) }

    LaunchedEffect(Unit) {
        runCatching { Api.loginUrl() }
            .onSuccess { url = it }
            .onFailure { failure = it.message }
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
            Icon(
                painterResource(R.drawable.ic_xbox),
                contentDescription = null,
                tint = c.textPrimary,
                modifier = Modifier.size(20.dp),
            )
            Spacer(Modifier.width(Space.sm))
            Text("Entrar na conta", style = Xau.type.rowTitle, color = c.textPrimary)
            Spacer(Modifier.weight(1f))
            GhostButton("Cancelar", onCancel)
        }

        when {
            failure != null -> StateBlock(
                title = "Não foi possível abrir o login",
                detail = failure.orEmpty(),
                actionLabel = "Voltar",
                onAction = onCancel,
                tone = c.danger,
            )

            url == null -> Column(
                Modifier.fillMaxWidth().padding(Space.gutter),
                verticalArrangement = Arrangement.spacedBy(Space.md),
            ) {
                Skeleton(220.dp, 24.dp)
                Skeleton(320.dp, 15.dp)
                Skeleton(280.dp, 15.dp)
            }

            else -> Box(Modifier.fillMaxSize()) {
                AndroidView(
                    modifier = Modifier.fillMaxSize(),
                    factory = { ctx ->
                        WebView(ctx).apply {
                            settings.javaScriptEnabled = true
                            settings.domStorageEnabled = true
                            // A WebView is white underneath whatever it
                            // renders. Microsoft's page does not fill the
                            // viewport, so the bottom third of the screen
                            // came out as a white slab in a dark app —
                            // caught by looking at a screenshot, invisible
                            // in the code.
                            setBackgroundColor(ink)
                            // A fresh jar per sign-in, so adding a second
                            // account does not silently reuse the first
                            // account's Microsoft session.
                            CookieManager.getInstance().removeAllCookies(null)

                            webViewClient = object : WebViewClient() {
                                override fun onPageStarted(
                                    view: WebView?,
                                    pageUrl: String?,
                                    favicon: android.graphics.Bitmap?,
                                ) {
                                    val u = pageUrl ?: return
                                    if (consumed) return
                                    scope.launch {
                                        runCatching { Api.onNavigate(u) }
                                            .onSuccess { code ->
                                                if (code != null && !consumed) {
                                                    consumed = true
                                                    onCode(code)
                                                }
                                            }
                                            .onFailure { e ->
                                                // Rust only errors here when
                                                // sign-in was refused or
                                                // cancelled, which is terminal.
                                                consumed = true
                                                onError(e.message ?: "login recusado")
                                            }
                                    }
                                }
                            }
                            loadUrl(url.orEmpty())
                        }
                    },
                )

                if (consumed) {
                    // The exchange runs against Xbox Live and takes a few
                    // seconds; without this the WebView sits on a blank
                    // redirect page looking hung.
                    Column(
                        Modifier
                            .fillMaxSize()
                            .background(c.ink)
                            .padding(Space.xl),
                        verticalArrangement = Arrangement.Center,
                        horizontalAlignment = Alignment.CenterHorizontally,
                    ) {
                        Text("Conectando à Xbox Live", style = Xau.type.section, color = c.textPrimary)
                        Spacer(Modifier.height(Space.sm))
                        Text(
                            "Validando o token e lendo o perfil.",
                            style = Xau.type.body,
                            color = c.textSecondary,
                        )
                    }
                }
            }
        }
    }
}
