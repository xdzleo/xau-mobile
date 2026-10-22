package com.xau.rewards.ui

import android.annotation.SuppressLint
import android.webkit.CookieManager
import android.webkit.JavascriptInterface
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
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import com.xau.rewards.R
import com.xau.rewards.core.Api
import com.xau.rewards.core.LoginOutcome
import com.xau.rewards.core.LoginPageAction
import com.xau.rewards.core.WindowsLoginPage
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import kotlinx.coroutines.launch

/**
 * Sign-in, as a Windows PC does it.
 *
 * The page is Microsoft's own "inline" sign-in, the one Windows shows in
 * its account settings, loaded with the headers and user agent of
 * Windows' web host. When it is done it hands its host a DA token
 * (`ServerData`) instead of redirecting with an OAuth code. Rust trades
 * that token, bound to this install's registered Windows device, for the
 * credential behind every xauth — the only kind Xbox accepts the presence
 * farm from. It is the sign-in the xodus / xodus-nexus Linux launcher
 * uses; see `windows_login.rs` in xau-mobile.
 *
 * The page talks to its host through `window.external.notify`, wired to
 * a JavaScript interface by a document-start script. **Rust decides what
 * every message means**: this screen only forwards messages and carries
 * out the reply (run a script, open a page, or finish).
 */
@SuppressLint("SetJavaScriptEnabled", "JavascriptInterface")
@Composable
fun LoginScreen(
    onDone: (LoginOutcome) -> Unit,
    onCancel: () -> Unit,
    onError: (String) -> Unit,
) {
    val c = Xau.colors
    // The WebView is a platform View, so it needs a plain ARGB int
    // rather than a Compose Color.
    val ink = c.ink.toArgb()
    val scope = rememberCoroutineScope()
    var page by remember { mutableStateOf<WindowsLoginPage?>(null) }
    var failure by remember { mutableStateOf<String?>(null) }
    // Set once the page handed over its token: the exchange with
    // Microsoft and Xbox Live takes a few seconds, and the page under it
    // is a blank post-back that would look hung.
    var working by remember { mutableStateOf(false) }
    // Terminal: after done or an error nothing more is forwarded.
    var finished by remember { mutableStateOf(false) }

    LaunchedEffect(Unit) {
        runCatching { Api.windowsLoginStart() }
            .onSuccess { page = it }
            .onFailure { failure = it.message }
    }

    val cancel = {
        finished = true
        scope.launch { runCatching { Api.windowsLoginCancel() } }
        onCancel()
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
            GhostButton("Cancelar", cancel)
        }

        val p = page
        when {
            failure != null -> StateBlock(
                title = "Não foi possível abrir o login",
                detail = failure.orEmpty(),
                actionLabel = "Voltar",
                onAction = onCancel,
                tone = c.danger,
            )

            p == null -> Column(
                Modifier.fillMaxWidth().padding(Space.gutter),
                verticalArrangement = Arrangement.spacedBy(Space.md),
            ) {
                // The first sign-in on this phone also registers it as a
                // Windows device with Microsoft, a few seconds more.
                Text(
                    "Preparando o login do Windows…",
                    style = Xau.type.body,
                    color = c.textSecondary,
                )
                Skeleton(220.dp, 24.dp)
                Skeleton(320.dp, 15.dp)
                Skeleton(280.dp, 15.dp)
            }

            else -> Box(Modifier.fillMaxSize()) {
                AndroidView(
                    modifier = Modifier.fillMaxSize(),
                    factory = { ctx ->
                        WebView(ctx).apply {
                            val view = this
                            settings.javaScriptEnabled = true
                            settings.domStorageEnabled = true
                            settings.userAgentString = p.userAgent
                            // A WebView is white underneath whatever it
                            // renders. Microsoft's page does not fill the
                            // viewport, so the bottom third of the screen
                            // came out as a white slab in a dark app.
                            setBackgroundColor(ink)
                            // A fresh jar per sign-in, so adding a second
                            // account does not silently reuse the first
                            // account's Microsoft session.
                            CookieManager.getInstance().removeAllCookies(null)

                            fun handle(message: String) {
                                if (finished) return
                                scope.launch {
                                    runCatching { Api.windowsLoginMessage(message) }
                                        .onSuccess { action ->
                                            when (action) {
                                                LoginPageAction.Ignore -> {}
                                                is LoginPageAction.Eval ->
                                                    view.evaluateJavascript(action.script, null)
                                                is LoginPageAction.Open -> {
                                                    // Microsoft wants one more
                                                    // page: it must be visible.
                                                    working = false
                                                    view.loadUrl(action.url)
                                                }
                                                is LoginPageAction.Done -> {
                                                    finished = true
                                                    onDone(action.outcome)
                                                }
                                            }
                                        }
                                        .onFailure { e ->
                                            finished = true
                                            onError(e.message ?: "login recusado")
                                        }
                                }
                            }

                            // Called on the WebView's JavaBridge thread;
                            // `handle` only launches on the main scope.
                            addJavascriptInterface(
                                object {
                                    @JavascriptInterface
                                    fun postMessage(message: String) {
                                        view.post { handle(message) }
                                    }
                                },
                                p.bridge,
                            )

                            val documentStart = WebViewFeature.isFeatureSupported(
                                WebViewFeature.DOCUMENT_START_SCRIPT,
                            )
                            if (documentStart) {
                                // Every origin, like the Windows host: the
                                // follow-up pages Microsoft may ask for are
                                // not always on login.live.com, and a bogus
                                // message only gets refused by Microsoft.
                                WebViewCompat.addDocumentStartJavaScript(
                                    this,
                                    p.initScript,
                                    setOf("*"),
                                )
                            }

                            webViewClient = object : WebViewClient() {
                                override fun onPageStarted(
                                    v: WebView?,
                                    pageUrl: String?,
                                    favicon: android.graphics.Bitmap?,
                                ) {
                                    // Old WebViews without document-start
                                    // scripts: early enough in practice,
                                    // the page only reports after the
                                    // user has typed.
                                    if (!documentStart) v?.evaluateJavascript(p.initScript, null)
                                }

                                override fun onPageFinished(v: WebView?, pageUrl: String?) {
                                    if (finished) return
                                    if (pageUrl?.startsWith(p.finishUrlPrefix) == true) {
                                        working = true
                                        v?.evaluateJavascript(p.finishScript, null)
                                    }
                                }
                            }
                            loadUrl(p.url, p.headers)
                        }
                    },
                )

                if (working) {
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
                            "Validando o login do Windows e lendo o perfil.",
                            style = Xau.type.body,
                            color = c.textSecondary,
                        )
                    }
                }
            }
        }
    }
}
