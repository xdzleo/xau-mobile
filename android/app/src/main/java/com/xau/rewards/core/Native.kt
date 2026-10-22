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

    // ---------------------------------------------- Windows sign-in

    /**
     * Open the Windows sign-in: what the WebView must load, with which
     * headers, user agent and scripts. The first call registers this
     * install as a Windows device with Microsoft (once, then stored).
     */
    suspend fun windowsLoginStart(): WindowsLoginPage {
        val r = obj("windows_login_start")
        val headers = r.getJSONArray("headers")
        return WindowsLoginPage(
            url = r.getString("url"),
            headers = (0 until headers.length()).associate {
                val pair = headers.getJSONArray(it)
                pair.getString(0) to pair.getString(1)
            },
            userAgent = r.getString("user_agent"),
            initScript = r.getString("init_script"),
            finishScript = r.getString("finish_script"),
            finishUrlPrefix = r.getString("finish_url_prefix"),
            bridge = r.getString("bridge"),
        )
    }

    /** Hand one bridge message from the page to the core. */
    suspend fun windowsLoginMessage(body: String): LoginPageAction {
        val r = obj("windows_login_message", JSONObject().put("body", body))
        return when (r.getString("action")) {
            "eval" -> LoginPageAction.Eval(r.getString("script"))
            "open" -> LoginPageAction.Open(r.getString("url"))
            "done" -> LoginPageAction.Done(
                LoginOutcome(
                    accounts = r.getJSONArray("accounts").toAccounts(),
                    warning = if (r.isNull("warning")) null else r.getString("warning"),
                ),
            )
            else -> LoginPageAction.Ignore
        }
    }

    suspend fun windowsLoginCancel() {
        obj("windows_login_cancel")
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
        arr("farm_titles").map {
            FarmTitle(
                titleId = it.getString("titleId"),
                name = it.getString("name"),
                image = if (it.isNull("image")) null else it.getString("image"),
            )
        }

    /** Every farm this process has run, from memory: no request is made. */
    suspend fun farmSummary(): List<FarmRun> =
        arr("farm_summary").map {
            FarmRun(
                xuid = it.getString("xuid"),
                gamertag = if (it.isNull("gamertag")) null else it.getString("gamertag"),
                running = it.getBoolean("running"),
                elapsed = it.optLong("elapsed"),
                total = it.optLong("total"),
                presence = if (it.isNull("presence")) null else it.getString("presence"),
            )
        }

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
            totalSecs = r.optLong("cycle_total_secs", 900L),
            presence = r.optJSONObject("presence")?.optString("status")?.takeIf { it.isNotEmpty() },
        )
    }

    /** One account's full mission list, read live. */
    suspend fun missions(xuid: String): List<Mission> =
        arr("missions", JSONObject().put("xuid", xuid)).map {
            Mission(
                id = it.getString("id"),
                title = it.optString("title"),
                points = it.optLong("points"),
                label = it.optString("label"),
                progress = it.optInt("progress"),
                done = it.optBoolean("done"),
                open = it.optBoolean("open"),
            )
        }

    // ---------------------------------------------------------- offers

    /** Every account's Game Pass offers, claimed or not. */
    suspend fun offersAll(): List<AccountOffers> =
        arr("offers_all").map { a ->
            AccountOffers(
                xuid = a.getString("xuid"),
                gamertag = a.optString("gamertag"),
                error = if (a.isNull("error")) null else a.getString("error"),
                offers = a.getJSONArray("offers").let { list ->
                    (0 until list.length()).map { i ->
                        val o = list.getJSONObject(i)
                        Offer(
                            offerId = o.getString("offerId"),
                            title = if (o.isNull("title")) null else o.getString("title"),
                            description = if (o.isNull("description")) null else o.getString("description"),
                            status = o.optString("status"),
                            resourceType = if (o.isNull("resourceType")) null else o.getString("resourceType"),
                            resource = if (o.isNull("resource")) null else o.getString("resource"),
                            claimedDate = if (o.isNull("claimedDate")) null else o.getString("claimedDate"),
                            available = o.optBoolean("available"),
                            claimed = o.optBoolean("claimed"),
                        )
                    }
                },
            )
        }

    /**
     * Claim every available offer across every account.
     *
     * IRREVERSIBLE. Each reward is written to its own CSV on the device
     * before it is reported here.
     */
    suspend fun claimOffersAll(): ClaimOffersResult {
        val r = obj("claim_offers_all")
        val claimed = r.getJSONArray("claimed").let { list ->
            (0 until list.length()).map { i ->
                val c = list.getJSONObject(i)
                ClaimedOffer(
                    gamertag = c.optString("gamertag"),
                    offerId = c.optString("offerId"),
                    title = if (c.isNull("title")) null else c.getString("title"),
                    resourceType = if (c.isNull("resourceType")) null else c.getString("resourceType"),
                    resource = if (c.isNull("resource")) null else c.getString("resource"),
                    csv = if (c.isNull("csv")) null else c.getString("csv"),
                )
            }
        }
        val fails = r.getJSONArray("failures").let { list ->
            (0 until list.length()).map { i ->
                val f = list.getJSONObject(i)
                "${f.optString("gamertag")}: ${f.optString("error")}"
            }
        }
        return ClaimOffersResult(
            claimed = claimed,
            failures = fails,
            indexCsv = if (r.isNull("indexCsv")) null else r.getString("indexCsv"),
        )
    }

    /** Codes recorded for one offer, newest first, across all accounts. */
    suspend fun offerCodes(offerId: String): List<OfferCode> =
        arr("offer_codes", JSONObject().put("offerId", offerId)).map {
            OfferCode(
                gamertag = it.optString("gamertag"),
                code = it.optString("code"),
                claimedDate = it.optString("claimedDate"),
            )
        }

    /**
     * The same rows as a file, with a filename safe to save under.
     *
     * [title] is what the screen is showing. The core can fall back to
     * the name inside the CSV, but that column is blank for anything
     * claimed before the content feed had a name for it — and a file
     * called after a GUID is a file nobody finds again.
     */
    suspend fun offerCsv(offerId: String, title: String): OfferCsvFile {
        val r = obj("offer_csv", JSONObject().put("offerId", offerId).put("title", title))
        return OfferCsvFile(fileName = r.getString("fileName"), text = r.getString("text"))
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
    /**
     * Rewards tier level. Null until the core reads `tiersProfile`,
     * which it does not yet — so this renders as an em dash rather than
     * as a confident "Nível 1" nobody verified.
     */
    val level: Long? = null,
    val farming: Boolean,
    /** Cycle progress 0-100 for THIS account's farm, when it has one. */
    val farmPercent: Float? = null,
    val farmElapsed: Long? = null,
    val farmTotal: Long? = null,
    /**
     * This account's farm, last ping: accepted / denied / rejected /
     * not_sent / unknown / pending. Null = no farm.
     */
    val farmPresence: String? = null,
    /** HTTP status Xbox answered that ping with, when it answered. */
    val farmPresenceHttp: Int? = null,
    /** How many activities are still earnable / already settled. */
    val missionsOpen: Int? = null,
    val missionsDone: Int? = null,
    /** Game Pass tier as the service names it. Null = no membership. */
    val gamepass: String? = null,
    /** Seen at sign-in. Null for accounts restored from an older store. */
    val email: String? = null,
    val active: Boolean,
    /** Why this row's figures are missing, when they are. */
    val staleReason: String?,
)

data class ClaimOutcome(val pointsAdded: Long, val accounts: List<Account>)

data class LoginOutcome(val accounts: List<Account>, val warning: String?)

/** Everything the login WebView needs; see `windows_login.rs`. */
data class WindowsLoginPage(
    val url: String,
    val headers: Map<String, String>,
    val userAgent: String,
    /** Run at the start of every document: wires `window.external`. */
    val initScript: String,
    /** Run once a page under [finishUrlPrefix] loads: hands over `ServerData`. */
    val finishScript: String,
    val finishUrlPrefix: String,
    /** Name the JavaScript interface must be registered under. */
    val bridge: String,
)

/** What the WebView does after the core read one page message. */
sealed interface LoginPageAction {
    data object Ignore : LoginPageAction
    data class Eval(val script: String) : LoginPageAction
    data class Open(val url: String) : LoginPageAction
    data class Done(val outcome: LoginOutcome) : LoginPageAction
}

data class Mission(
    val id: String,
    val title: String,
    val points: Long,
    /** The service's own wording: Available, Claimed, Locked, … */
    val label: String,
    /** The service's own 0-100 figure, not an estimate of ours. */
    val progress: Int,
    val done: Boolean,
    val open: Boolean,
)

data class Offer(
    val offerId: String,
    /** From the content feed. Null when it had nothing for this offer. */
    val title: String?,
    val description: String?,
    val status: String,
    /** `external-link`, `store-token`, … Treated as opaque. */
    val resourceType: String?,
    /** The reward. Null until claimed — claiming is what mints it. */
    val resource: String?,
    val claimedDate: String?,
    val available: Boolean,
    val claimed: Boolean,
)

data class AccountOffers(
    val xuid: String,
    val gamertag: String,
    val offers: List<Offer>,
    /** Set when this account's list could not be read; others still load. */
    val error: String?,
)

data class ClaimedOffer(
    val gamertag: String,
    val offerId: String,
    val title: String?,
    val resourceType: String?,
    val resource: String?,
    /** Where the reward was written on disk. */
    val csv: String?,
)

data class ClaimOffersResult(
    val claimed: List<ClaimedOffer>,
    val failures: List<String>,
    val indexCsv: String?,
)

data class OfferCode(
    val gamertag: String,
    /** The link or store token the claim minted. */
    val code: String,
    val claimedDate: String,
)

data class OfferCsvFile(
    val fileName: String,
    val text: String,
)

/** One account's farm as the foreground service tracks it. */
data class FarmRun(
    val xuid: String,
    val gamertag: String?,
    val running: Boolean,
    val elapsed: Long,
    val total: Long,
    /** accepted / denied / rejected / not_sent / unknown. */
    val presence: String?,
) {
    /** Ran its whole cycle, as opposed to being switched off early. */
    val completed: Boolean get() = !running && total > 0 && elapsed >= total - 5
}

/** One of the farm's titles; `image` is its box art, when titlehub had one. */
data class FarmTitle(val titleId: String, val name: String, val image: String? = null)

data class FarmState(
    val state: String,
    val percent: Float,
    val cycle: Long,
    val pings: Long,
    val elapsedSecs: Long,
    val totalSecs: Long,
    /** Last ping's outcome: accepted / denied / rejected / not_sent / unknown / pending. */
    val presence: String? = null,
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
        level = if (o.isNull("level")) null else o.getLong("level"),
        farming = o.optBoolean("farming"),
        farmPercent = if (o.isNull("farmPercent")) null else o.getDouble("farmPercent").toFloat(),
        farmElapsed = if (o.isNull("farmElapsed")) null else o.getLong("farmElapsed"),
        farmTotal = if (o.isNull("farmTotal")) null else o.getLong("farmTotal"),
        farmPresence = if (o.isNull("farmPresence")) null else o.getString("farmPresence"),
        farmPresenceHttp = if (o.isNull("farmPresenceHttp")) null else o.getInt("farmPresenceHttp"),
        missionsOpen = if (o.isNull("missionsOpen")) null else o.getInt("missionsOpen"),
        missionsDone = if (o.isNull("missionsDone")) null else o.getInt("missionsDone"),
        gamepass = if (o.isNull("gamepass")) null else o.getString("gamepass"),
        email = if (o.isNull("email")) null else o.getString("email"),
        active = o.optBoolean("active"),
        staleReason = if (o.isNull("staleReason")) null else o.getString("staleReason"),
    )
}
