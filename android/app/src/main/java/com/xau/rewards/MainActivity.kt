package com.xau.rewards

import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Icon
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.withStyle
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
import com.xau.rewards.ui.AccountDetail
import com.xau.rewards.ui.HomeScreen
import com.xau.rewards.ui.LogScreen
import com.xau.rewards.ui.LoginScreen
import com.xau.rewards.ui.OffersScreen
import com.xau.rewards.ui.SettingsScreen
import com.xau.rewards.ui.StoreScreen
import com.xau.rewards.ui.StateBlock
import com.xau.rewards.ui.entrance
import com.xau.rewards.ui.theme.Radius
import com.xau.rewards.ui.theme.Space
import com.xau.rewards.ui.theme.Xau
import com.xau.rewards.ui.theme.XauTheme
import kotlinx.coroutines.delay

class MainActivity : ComponentActivity() {

    private val vm: AppViewModel by viewModels()

    /**
     * Android 13+ shows no notification at all until the user allows it,
     * the farm's progress and its "completed" notice included. Asked when
     * a farm is switched on, the moment the notifications start to matter.
     */
    private val notificationPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) {}

    private fun askForNotifications() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            notificationPermission.launch(android.Manifest.permission.POST_NOTIFICATIONS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        // Called before setContent so the first frame already draws
        // under the bars — otherwise the layout visibly jumps once the
        // insets arrive.
        //
        // Both bars are pinned to a DARK style explicitly. The default
        // follows the system theme, which on a phone set to light gave
        // this dark app a white navigation bar across the bottom —
        // visible only on a real device, because a JVM renderer resolves
        // insets to zero and never draws the bars at all.
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        preferHighestRefreshRate()

        setContent {
            XauTheme {
                val state by vm.state.collectAsStateWithLifecycle()
                var loggingIn by remember { mutableStateOf(false) }
                var openAccount by remember { mutableStateOf<String?>(null) }
                var storeOf by remember { mutableStateOf<String?>(null) }
                var showOffers by remember { mutableStateOf(false) }
                var showSettings by remember { mutableStateOf(false) }
                var showLog by remember { mutableStateOf(false) }

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
                            // Only leave the login screen once the exchange
                            // landed; dropping out first showed the empty
                            // state while the account was still on its way.
                            onDone = { out ->
                                loggingIn = false
                                vm.onLoggedIn(out.accounts, out.warning)
                            },
                            onCancel = { loggingIn = false },
                            // A sign-in that failed after Microsoft accepted
                            // the password must not look like one that
                            // never happened.
                            onError = { msg ->
                                loggingIn = false
                                vm.reportLoginFailure(Exception(msg))
                            },
                        )

                        showOffers -> OffersScreen(onBack = { showOffers = false })

                        showSettings -> SettingsScreen(
                            onBack = { showSettings = false },
                            singleActive = state.singleActive,
                            onSingleActive = vm::setSingleActive,
                        )
                        showLog -> LogScreen(onBack = { showLog = false })

                        openAccount != null && state.accounts.any { it.xuid == openAccount } ->
                            AccountDetail(
                                account = state.accounts.first { it.xuid == openAccount },
                                onBack = { openAccount = null },
                            )

                        storeOf != null && state.accounts.any { it.xuid == storeOf } ->
                            StoreScreen(
                                account = state.accounts.first { it.xuid == storeOf },
                                onBack = { storeOf = null },
                                onOpenMissions = { openAccount = storeOf },
                                onRefresh = { vm.refresh(quiet = true) },
                            )

                        !state.booting && !state.signedIn ->
                            Welcome(error = state.fatal) { loggingIn = true }

                        else -> HomeScreen(
                            state = state,
                            onClaim = vm::claim,
                            onSelect = vm::select,
                            onSignOut = vm::signOut,
                            onAddAccount = { loggingIn = true },
                            onToggleFarm = { xuid ->
                                val switchingOn = state.accounts.none { it.xuid == xuid && it.farming }
                                if (switchingOn) askForNotifications()
                                vm.toggleFarm(xuid)
                            },
                            onOpenAccount = { openAccount = it.xuid },
                            onOpenStore = { storeOf = it.xuid },
                            onOpenOffers = { showOffers = true },
                            onOpenSettings = { showSettings = true },
                            onOpenLog = { showLog = true },
                            onRetry = { vm.refresh() },
                            onNextFarmPool = vm::nextFarmPool,
                            onToggleFrozen = vm::toggleFrozen,
                        )
                    }

                    state.notice?.let { Notice(it, vm::dismissNotice) }
                }
            }
        }
    }

    /**
     * Ask for the panel's fastest mode (120 Hz on a moto g34) at the
     * current resolution.
     *
     * Many phones run apps at 60 Hz unless the window says otherwise, so
     * the scrolling and the farm's animations would never get the
     * smoothness the screen has. Same resolution only: switching size
     * would be a visible mode change, not a refresh-rate one.
     */
    private fun preferHighestRefreshRate() {
        @Suppress("DEPRECATION")
        val screen: android.view.Display = (
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) display else windowManager.defaultDisplay
            ) ?: return
        val current = screen.mode
        val best = screen.supportedModes
            .filter {
                it.physicalWidth == current.physicalWidth &&
                    it.physicalHeight == current.physicalHeight
            }
            .maxByOrNull { it.refreshRate } ?: return
        window.attributes = window.attributes.apply {
            preferredDisplayModeId = best.modeId
            preferredRefreshRate = best.refreshRate
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
    val c = Xau.colors
    Column(
        Modifier
            .fillMaxSize()
            .windowInsetsPadding(WindowInsets.safeDrawing)
            .padding(horizontal = Space.xxl),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        // The first screen anyone sees said nothing about what this is:
        // a small block of grey text floating in a black void, with the
        // one action on it rendered as the quietest element present.
        Box(
            Modifier.entrance(0, rise = 24.dp).size(88.dp).clip(RoundedCornerShape(Radius.pill)).background(Color.White),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painterResource(R.drawable.ic_xbox),
                contentDescription = null,
                tint = c.greenDeep,
                modifier = Modifier.size(60.dp),
            )
        }

        Spacer(Modifier.height(Space.xxl))

        Text(
            buildAnnotatedString {
                append("Xbox ")
                withStyle(SpanStyle(color = c.green)) { append("Rewards") }
            },
            style = Xau.type.hero.copy(fontSize = Xau.type.brand.fontSize * 1.4f),
            color = c.textPrimary,
            modifier = Modifier.entrance(1),
        )
        Text(
            "Jogos. Presença. Recompensas.",
            style = Xau.type.body,
            color = c.textSecondary,
            modifier = Modifier.entrance(2),
        )

        Spacer(Modifier.height(Space.huge))

        Text(
            error ?: "Entre com a sua conta Microsoft para ver o saldo do Rewards, " +
                "resgatar os pontos pendentes e rodar o farm de presença.",
            style = Xau.type.body,
            color = if (error != null) c.danger else c.textSecondary,
            textAlign = TextAlign.Center,
        )

        Spacer(Modifier.height(Space.xxl))

        // Filled, not outlined. This is the only thing the user can do
        // on this screen; it has no business being the quietest mark on
        // it.
        Box(
            Modifier
                .entrance(4)
                .fillMaxWidth()
                .height(Space.touch + 6.dp)
                .clip(RoundedCornerShape(Radius.control))
                .background(c.greenDeep)
                .clickable(onClick = onAdd),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                if (error != null) "Tentar de novo" else "Entrar com a Microsoft",
                style = Xau.type.rowTitle,
                color = c.onGreen,
            )
        }

        Spacer(Modifier.height(Space.lg))
        Text(
            "Nenhum dado sai deste aparelho além das chamadas à Xbox Live.",
            style = Xau.type.caption,
            color = c.textMuted,
            textAlign = TextAlign.Center,
        )
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
