package com.xau.rewards

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.xau.rewards.core.Account
import com.xau.rewards.core.Api
import com.xau.rewards.core.FarmState
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch

/**
 * Everything the screen shows, and the only place it changes.
 *
 * Rust owns the sessions, the balances and the farm. This holds no
 * authoritative state — it caches the last answer so the UI has
 * something to draw, and forwards taps.
 */
data class UiState(
    val booting: Boolean = true,
    val accounts: List<Account> = emptyList(),
    val farm: FarmState? = null,
    val publicIp: String? = null,
    val refreshing: Boolean = false,
    val claimingXuid: String? = null,
    /** A failure that stopped the whole screen, not one row. */
    val fatal: String? = null,
    /** Transient confirmation of something that just happened. */
    val notice: String? = null,
) {
    val active: Account? get() = accounts.firstOrNull { it.active } ?: accounts.firstOrNull()
    val signedIn: Boolean get() = accounts.isNotEmpty()
    val totalWaiting: Long get() = accounts.sumOf { it.unclaimed ?: 0L }
}

class AppViewModel(app: Application) : AndroidViewModel(app) {

    private val _state = MutableStateFlow(UiState())
    val state: StateFlow<UiState> = _state.asStateFlow()

    private var poller: Job? = null

    /**
     * Auto-claim fires without a tap and cannot be undone, so it is
     * rate-limited rather than retried: a sweep that credits nothing was
     * refused service-side, and asking again a minute later will not
     * change the answer.
     */
    private var autoBlockedUntilMs = 0L
    private var autoClaiming = false

    init {
        viewModelScope.launch {
            runCatching { Api.restore() }
                .onSuccess { _state.update { s -> s.copy(accounts = it, booting = false) } }
                .onFailure { e -> _state.update { s -> s.copy(booting = false, fatal = e.message) } }
            if (_state.value.signedIn) refresh()
            loadIp()
        }
    }

    // ------------------------------------------------------------ poll

    /** Called from the composition while the screen is resumed. */
    fun startPolling() {
        if (poller?.isActive == true) return
        poller = viewModelScope.launch {
            while (true) {
                delay(POLL_MS)
                if (_state.value.signedIn) refresh(quiet = true)
            }
        }
    }

    fun stopPolling() {
        poller?.cancel()
        poller = null
    }

    // --------------------------------------------------------- accounts

    fun refresh(quiet: Boolean = false) = viewModelScope.launch {
        if (!quiet) _state.update { it.copy(refreshing = true) }
        runCatching { Api.refresh() }
            .onSuccess { rows ->
                _state.update { it.copy(accounts = rows, refreshing = false, fatal = null) }
                refreshFarm()
                maybeAutoClaim()
            }
            .onFailure { e ->
                // A failed refresh must not leave stale figures on screen
                // looking current, so the rows Rust returns already carry
                // null balances. Only surface the message when the user
                // asked for the refresh.
                _state.update {
                    it.copy(refreshing = false, fatal = if (quiet) it.fatal else e.message)
                }
            }
    }

    fun select(xuid: String) = viewModelScope.launch {
        runCatching { Api.select(xuid) }.onSuccess {
            _state.update { s ->
                s.copy(accounts = s.accounts.map { it.copy(active = it.xuid == xuid) })
            }
            refreshFarm()
        }
    }

    fun signOut(xuid: String) = viewModelScope.launch {
        runCatching { Api.signOut(xuid) }
            .onSuccess { rows -> _state.update { it.copy(accounts = rows, notice = "Conta removida") } }
            .onFailure { e -> _state.update { it.copy(fatal = e.message) } }
    }

    fun onLoggedIn(accounts: List<Account>, warning: String? = null) {
        _state.update {
            it.copy(
                accounts = accounts,
                fatal = null,
                notice = warning ?: "Conta conectada",
            )
        }
        refresh()
    }

    /**
     * Surface a sign-in that failed after Microsoft accepted the
     * password.
     *
     * Without this the screen fell back to "no accounts connected",
     * which is what a sign-in that never started also looks like — so
     * the one case the user needed to distinguish was the one case they
     * could not.
     */
    fun reportLoginFailure(e: Throwable) {
        _state.update { it.copy(fatal = e.message ?: "falha ao conectar a conta") }
    }

    // ---------------------------------------------------------- rewards

    fun claim(xuid: String) = viewModelScope.launch {
        _state.update { it.copy(claimingXuid = xuid) }
        runCatching { Api.claim(xuid) }
            .onSuccess { out ->
                _state.update {
                    it.copy(
                        accounts = out.accounts,
                        claimingXuid = null,
                        notice = if (out.pointsAdded > 0) {
                            "+${out.pointsAdded} pontos resgatados"
                        } else {
                            "Nada foi creditado"
                        },
                    )
                }
            }
            .onFailure { e ->
                _state.update { it.copy(claimingXuid = null, notice = e.message) }
            }
    }

    /**
     * Claim wherever points are waiting, without a tap.
     *
     * One pass at a time, and a 30-minute pause after a sweep that
     * credits nothing or errors — both mean the service said no, and
     * hammering it every minute would only look like abuse.
     */
    private suspend fun maybeAutoClaim() {
        if (autoClaiming || System.currentTimeMillis() < autoBlockedUntilMs) return
        if (_state.value.totalWaiting <= 0L) return

        autoClaiming = true
        try {
            val out = Api.claimAll()
            if (out.pointsAdded > 0) {
                _state.update {
                    it.copy(accounts = out.accounts, notice = "+${out.pointsAdded} resgatados automaticamente")
                }
            } else {
                autoBlockedUntilMs = System.currentTimeMillis() + AUTO_BACKOFF_MS
                _state.update {
                    it.copy(accounts = out.accounts, notice = "Resgate automático: nada creditado — pausado 30 min")
                }
            }
        } catch (e: Exception) {
            autoBlockedUntilMs = System.currentTimeMillis() + AUTO_BACKOFF_MS
            _state.update { it.copy(notice = "Resgate automático falhou — pausado 30 min") }
        } finally {
            autoClaiming = false
        }
    }

    // ------------------------------------------------------------- farm

    fun toggleFarm() = viewModelScope.launch {
        val xuid = _state.value.active?.xuid ?: return@launch
        val running = _state.value.farm?.running == true
        runCatching { if (running) Api.farmStop(xuid) else Api.farmStart(xuid) }
            .onSuccess {
                // The foreground service follows the farm, not the tap:
                // starting it before the core confirmed would leave a
                // notification advertising a farm that failed to start.
                val ctx = getApplication<Application>()
                if (running) FarmService.stop(ctx) else FarmService.start(ctx)
                refreshFarm()
            }
            .onFailure { e -> _state.update { it.copy(notice = e.message) } }
    }

    private suspend fun refreshFarm() {
        val xuid = _state.value.active?.xuid ?: return
        runCatching { Api.farmState(xuid) }
            .onSuccess { f -> _state.update { it.copy(farm = f) } }
    }

    // ------------------------------------------------------------- misc

    private fun loadIp() = viewModelScope.launch {
        runCatching { Api.publicIp() }
            .onSuccess { ip -> _state.update { it.copy(publicIp = ip) } }
        // A failed lookup stays null: the row simply does not appear,
        // rather than claiming an address we could not confirm.
    }

    fun dismissNotice() = _state.update { it.copy(notice = null) }

    private companion object {
        const val POLL_MS = 60_000L
        const val AUTO_BACKOFF_MS = 30 * 60_000L
    }
}
