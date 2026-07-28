package com.xau.rewards.core

import org.json.JSONArray
import org.json.JSONObject

/**
 * The raw JNI surface: one call in, one JSON string out.
 *
 * Nothing outside this file should touch it — [Api] wraps every method
 * in a typed function so a rename in Rust breaks at one place instead of
 * at whichever screen happened to call it.
 */
internal object Native {
    @Volatile
    private var started = false

    /**
     * Load the library and hand the core a writable directory.
     *
     * Must run before any [call]. Rust cannot work the path out itself
     * on Android — its directory crate falls back to `$HOME/.local/share`,
     * which for an app process is `/` and is not writable.
     */
    fun start(filesDir: String) {
        if (started) return
        synchronized(this) {
            if (started) return
            System.loadLibrary("xau_jni")
            init(filesDir)
            started = true
        }
    }

    external fun init(dataDir: String)

    external fun call(method: String, args: String): String?
}

/** A failure the Rust side reported, carrying its own wording. */
class XauException(message: String) : Exception(message)

/**
 * Typed access to the core.
 *
 * Every call blocks on network work, so all of these are suspending and
 * hop to [kotlinx.coroutines.Dispatchers.IO]. Calling one from the main
 * thread would freeze the frame the user is looking at.
 */
object Api {
    private suspend fun invoke(method: String, args: JSONObject = JSONObject()): Any =
        kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
            val raw = Native.call(method, args.toString())
                ?: throw XauException("o núcleo não respondeu")
            val env = JSONObject(raw)
            if (env.has("err")) throw XauException(env.getString("err"))
            // `ok` is whatever the method returns — an object, an array,
            // or a bare value. Callers know which shape they asked for.
            env.get("ok")
        }

    private suspend fun obj(method: String, args: JSONObject = JSONObject()): JSONObject =
        invoke(method, args) as JSONObject

    private suspend fun arr(method: String, args: JSONObject = JSONObject()): JSONArray =
        invoke(method, args) as JSONArray

    // ------------------------------------------------------------ auth

    suspend fun loginUrl(): String = obj("login_url").getString("url")

    /**
     * Classify a URL the login WebView reached.
     *
     * Returns the authorization code once sign-in completes, or null
     * while the user is still on Microsoft's pages. Must be called for
     * every navigation: only the real redirect endpoint is terminal, and
     * treating a bare `?code=` as success would let any page reached
     * mid-flow hand over a code.
     */
    suspend fun onNavigate(url: String): String? {
        val r = obj("on_navigate", JSONObject().put("url", url))
        return if (r.getBoolean("done")) r.getString("code") else null
    }

    /**
     * Exchange the authorization code for a live session.
     *
     * [LoginOutcome.warning] is set when the account signed in but could
     * not be written to disk — it works now and will be gone after a
     * restart, and the user is entitled to know which of those is true.
     */
    suspend fun finishLogin(code: String): LoginOutcome {
        val r = obj("finish_login", JSONObject().put("code", code))
        return LoginOutcome(
            accounts = r.getJSONArray("accounts").toAccounts(),
            warning = if (r.isNull("warning")) null else r.getString("warning"),
        )
    }

    suspend fun signOut(xuid: String): List<Account> =
        arr("sign_out", JSONObject().put("xuid", xuid)).toAccounts()

    // -------------------------------------------------------- accounts

    suspend fun restore(): List<Account> = arr("restore").toAccounts()

    suspend fun refresh(): List<Account> = arr("refresh").toAccounts()

    suspend fun select(xuid: String) {
        obj("select", JSONObject().put("xuid", xuid))
    }

    // --------------------------------------------------------- rewards

    suspend fun claim(xuid: String): ClaimOutcome {
        val r = obj("claim", JSONObject().put("xuid", xuid))
        return ClaimOutcome(
            pointsAdded = r.optLong("pointsAdded"),
            accounts = r.getJSONArray("accounts").toAccounts(),
        )
    }

    suspend fun claimAll(): ClaimOutcome {
        val r = obj("claim_all")
        return ClaimOutcome(
            pointsAdded = r.optLong("pointsAdded"),
            accounts = r.getJSONArray("accounts").toAccounts(),
        )
    }

    // ------------------------------------------------------------ farm

    suspend fun farmTitles(): List<FarmTitle> =
        arr("farm_titles").map { FarmTitle(it.getString("titleId"), it.getString("name")) }

    suspend fun farmStart(xuid: String) {
        obj("farm_start", JSONObject().put("xuid", xuid))
    }

    suspend fun farmStop(xuid: String) {
        obj("farm_stop", JSONObject().put("xuid", xuid))
    }

    suspend fun farmState(xuid: String): FarmState {
        val r = obj("farm_state", JSONObject().put("xuid", xuid))
        return FarmState(
            state = r.optString("state", "Idle"),
            percent = r.optDouble("percent", 0.0).toFloat(),
            cycle = r.optLong("cycle"),
            pings = r.optLong("ping_count"),
            elapsedSecs = r.optLong("cycle_elapsed_secs"),
            totalSecs = r.optLong("cycle_total_secs", 960L),
        )
    }

    // ------------------------------------------------------------ misc

    suspend fun publicIp(): String = obj("public_ip").getString("ip")
}

// ------------------------------------------------------------- models

data class Account(
    val xuid: String,
    val gamertag: String,
    val gamerpic: String,
    val gamerscore: Long,
    /** Null until a read lands, and null again when one fails. */
    val balance: Long?,
    val unclaimed: Long?,
    val farming: Boolean,
    val active: Boolean,
    /** Why this row's figures are missing, when they are. */
    val staleReason: String?,
)

data class ClaimOutcome(val pointsAdded: Long, val accounts: List<Account>)

data class LoginOutcome(val accounts: List<Account>, val warning: String?)

data class FarmTitle(val titleId: String, val name: String)

data class FarmState(
    val state: String,
    val percent: Float,
    val cycle: Long,
    val pings: Long,
    val elapsedSecs: Long,
    val totalSecs: Long,
) {
    val running: Boolean get() = state == "Farming" || state == "Pinging"
}

// ------------------------------------------------------------ decoding

private inline fun <T> JSONArray.map(f: (JSONObject) -> T): List<T> =
    (0 until length()).map { f(getJSONObject(it)) }

private fun JSONArray.toAccounts(): List<Account> = map { o ->
    Account(
        xuid = o.getString("xuid"),
        gamertag = o.optString("gamertag"),
        gamerpic = o.optString("gamerpic"),
        gamerscore = o.optLong("gamerscore"),
        // `optLong` cannot distinguish absent from zero, and the whole
        // point of these being nullable is that "never fetched" must not
        // render as a real 0.
        balance = if (o.isNull("balance")) null else o.getLong("balance"),
        unclaimed = if (o.isNull("unclaimed")) null else o.getLong("unclaimed"),
        farming = o.optBoolean("farming"),
        active = o.optBoolean("active"),
        staleReason = if (o.isNull("staleReason")) null else o.getString("staleReason"),
    )
}
