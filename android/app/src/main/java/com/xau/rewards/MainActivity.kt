package com.xau.rewards

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.lifecycleScope
import com.xau.rewards.core.Api
import com.xau.rewards.ui.HomeScreen
import com.xau.rewards.ui.LoginScreen
import com.xau.rewards.ui.StateBlock
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import com.xau.rewards.ui.theme.XauTheme
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {

    private val vm: AppViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        // Called before setContent so the first frame already draws
        // under the bars — otherwise the layout visibly jumps once the
        // insets arrive.
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)

        setContent {
            XauTheme {
                val state by vm.state.collectAsStateWithLifecycle()
                var loggingIn by remember { mutableStateOf(false) }

                // Polling only while the screen is actually in front of
                // the user; a background poll costs battery to refresh
                // pixels nobody is looking at.
                LifecycleResumeEffect(Unit) {
                    vm.startPolling()
                    onPauseOrDispose { vm.stopPolling() }
                }

                Box(Modifier.fillMaxSize().background(Xau.colors.ink)) {
                    when {
                        loggingIn -> LoginScreen(
                            onCode = { code ->
                                lifecycleScope.launch {
                                    runCatching { Api.finishLogin(code) }
                                        .onSuccess { out ->
                                            // Only leave the login screen
                                            // once the exchange landed;
                                            // dropping out first showed the
                                            // empty state while the account
                                            // was still on its way.
                                            loggingIn = false
                                            vm.onLoggedIn(out.accounts, out.warning)
                                        }
                                        .onFailure { e ->
                                            // Was silently swallowed. A
                                            // sign-in that failed after
                                            // Microsoft accepted the
                                            // password looked exactly like
                                            // one that never happened.
                                            loggingIn = false
                                            vm.reportLoginFailure(e)
                                        }
                                }
                            },
                            onCancel = { loggingIn = false },
                            onError = { msg ->
                                loggingIn = false
                                vm.reportLoginFailure(Exception(msg))
                            },
                        )

                        !state.booting && !state.signedIn ->
                            Welcome(error = state.fatal) { loggingIn = true }

                        else -> HomeScreen(
                            state = state,
                            onClaim = vm::claim,
                            onSelect = vm::select,
                            onSignOut = vm::signOut,
                            onAddAccount = { loggingIn = true },
                            onToggleFarm = vm::toggleFarm,
                        )
                    }

                    state.notice?.let { Notice(it, vm::dismissNotice) }
                }
            }
        }
    }
}

/**
 * First run: nothing is signed in yet.
 *
 * `DESIGN.md` §7 — an empty screen must say what goes here and give the
 * action that fills it.
 */
@Composable
private fun Welcome(error: String?, onAdd: () -> Unit) {
    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        // When a sign-in failed, this screen must say so. Without the
        // error it is indistinguishable from never having tried — which
        // is exactly what made a broken login look like nothing at all
        // had happened.
        if (error != null) {
            StateBlock(
                title = "Não foi possível conectar",
                detail = error,
                actionLabel = "Tentar de novo",
                onAction = onAdd,
                tone = Xau.colors.danger,
            )
        } else {
            StateBlock(
                title = "Nenhuma conta conectada",
                detail = "Entre com a sua conta Microsoft para ver o saldo do Rewards, " +
                    "resgatar os pontos pendentes e rodar o farm de presença.",
                actionLabel = "Entrar",
                onAction = onAdd,
            )
        }
    }
}

/**
 * A transient confirmation.
 *
 * Anchored to the bottom and self-dismissing: the claim it reports has
 * already happened, so it never needs an acknowledgement.
 */
@Composable
private fun Notice(text: String, onDismiss: () -> Unit) {
    LaunchedEffect(text) {
        delay(3_500)
        onDismiss()
    }
    Box(
        Modifier
            .fillMaxSize()
            .windowInsetsPadding(WindowInsets.safeDrawing)
            .padding(Space.gutter),
        contentAlignment = Alignment.BottomCenter,
    ) {
        Box(
            Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(Radius.control))
                .background(Xau.colors.surfaceHigh)
                .padding(horizontal = Space.lg, vertical = Space.md),
        ) {
            Text(text, style = Xau.type.body, color = Xau.colors.textPrimary)
        }
    }
}
