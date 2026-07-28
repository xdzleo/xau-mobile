package com.xau.rewards

import app.cash.paparazzi.DeviceConfig
import app.cash.paparazzi.Paparazzi
import com.xau.rewards.core.Account
import com.xau.rewards.core.FarmState
import com.xau.rewards.ui.HomeScreen
import com.xau.rewards.ui.theme.XauTheme
import org.junit.Rule
import org.junit.Test

/**
 * Renders the screens to PNG on the JVM, so the design can be reviewed
 * without an emulator.
 *
 * Every real design defect found in this app so far was found by
 * LOOKING at it and none by reading the code, so this is the loop that
 * matters. Run:
 *
 *     build-apk.cmd :app:recordPaparazziDebug
 *
 * `record`, never `verify` — a critique loop wants the image regenerated
 * unconditionally, not compared to a golden.
 *
 * # What this cannot see
 *
 * - **Insets.** `WindowInsets.safeDrawing` resolves to zero here, so
 *   status-bar and nav-bar clearance is NOT tested. Check that against a
 *   real capture (`scripts/shoot.sh`).
 * - **The JNI bridge.** `XauApp` never runs, so `System.loadLibrary` is
 *   never called and a broken core is invisible.
 * - Scroll, fling, animation timing, real font hinting, Coil decode.
 *
 * There is deliberately NO test for `LoginScreen`: it calls
 * `Api.loginUrl()` from a `LaunchedEffect`, which throws
 * `UnsatisfiedLinkError` with no native library loaded. Its own
 * `runCatching` swallows that, so it would render its *error* state and
 * look like a real screen.
 */
class DesignSnapshots {

    @get:Rule
    val paparazzi = Paparazzi(
        deviceConfig = DeviceConfig.PIXEL_6.copy(softButtons = false),
        showSystemUi = false,
    )

    // Real content, per DESIGN.md §9: a fake "User 1" hides exactly the
    // layout problems that show up in production.
    private fun account(
        xuid: String,
        tag: String,
        balance: Long?,
        waiting: Long?,
        active: Boolean,
        farming: Boolean = false,
        stale: String? = null,
    ) = Account(
        xuid = xuid,
        gamertag = tag,
        gamerpic = "",
        gamerscore = 48_215,
        balance = balance,
        unclaimed = waiting,
        farming = farming,
        active = active,
        staleReason = stale,
    )

    // Distinct xuids matter: it is the LazyColumn key, and duplicates
    // throw at measure time rather than rendering wrong.
    private val loaded = UiState(
        booting = false,
        accounts = listOf(
            account("2533274801234567", "xdzleo", 18_420, 350, active = true, farming = true),
            account("2533274809999999", "DimplyWig8972", 2_110, null, active = false),
        ),
        farm = FarmState("Farming", 42f, 3, 128, 610, 900),
        publicIp = "189.4.22.10",
    )

    private fun render(state: UiState) = paparazzi.snapshot {
        XauTheme {
            HomeScreen(
                state = state,
                onClaim = {},
                onSelect = {},
                onSignOut = {},
                onAddAccount = {},
                onToggleFarm = {},
            )
        }
    }

    @Test
    fun home() = render(loaded)

    /** Skeletons must match the shape of what replaces them. */
    @Test
    fun booting() = render(UiState(booting = true))

    /** A read that failed clears its figures rather than leaving stale ones. */
    @Test
    fun stale() = render(
        loaded.copy(
            accounts = listOf(
                account(
                    "2533274801234567", "xdzleo", null, null,
                    active = true, stale = "sem resposta do Xbox",
                ),
            ),
        ),
    )
}
