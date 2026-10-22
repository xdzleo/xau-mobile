//! The bridge between Kotlin and the Rust core.
//!
//! # One function, not thirty
//!
//! Every operation crosses as `call(method, argsJson) -> resultJson`.
//! The alternative — one `extern "system"` symbol per operation — means
//! a hand-written signature, a hand-written Kotlin `external fun`, and a
//! hand-written type conversion for each, all of which fail at runtime
//! rather than at compile time when they drift apart. Collapsing to one
//! symbol puts every conversion in `serde_json`, which does not drift,
//! and keeps `unsafe` confined to a single entry point.
//!
//! The cost is that method names are strings resolved at runtime. That
//! is contained by [`dispatch`] returning a plain error for an unknown
//! name, and by the Kotlin side wrapping each call in a typed function.
//!
//! # Errors cross as data
//!
//! A Rust panic unwinding into the JVM is undefined behaviour, so the
//! entry point catches unwinds and every failure comes back as
//! `{"err": "..."}`. Kotlin turns that into an exception on its side of
//! the fence, where an exception is safe.

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock};

use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::JNIEnv;
use serde_json::{json, Value};

use xau_core::error::{Error, Result};
use xau_mobile::accounts::{self, Store};
use xau_mobile::{FarmHandle, Session};

// ------------------------------------------------------------- runtime

/// Sessions are rebuilt on launch and reused for the life of the
/// process; the farm handles outlive the screen that started them.
#[derive(Default)]
struct State {
    sessions: HashMap<String, Arc<Session>>,
    farms: HashMap<String, FarmHandle>,
    /// Which account the UI is currently showing.
    active: Option<String>,
    /// Accounts whose last refresh failed. Their figures are cleared
    /// rather than left on screen looking current.
    stale: HashMap<String, String>,
    /// Frozen accounts (mirrors `Store::frozen`): no request of any kind
    /// goes out for them. Every path to the network for an account passes
    /// through [`State::live_sessions`] or [`session_of`], which skip them.
    frozen: HashSet<String>,
}

impl State {
    /// The sessions requests may go out for: every one but the frozen.
    fn live_sessions(&self) -> Vec<Arc<Session>> {
        self.sessions
            .iter()
            .filter(|(x, _)| !self.frozen.contains(*x))
            .map(|(_, s)| s.clone())
            .collect()
    }

    fn first_live(&self) -> Option<String> {
        self.sessions.keys().find(|x| !self.frozen.contains(*x)).cloned()
    }
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn runtime() -> Result<&'static tokio::runtime::Runtime> {
    static RT: OnceLock<std::io::Result<tokio::runtime::Runtime>> = OnceLock::new();
    // Two worker threads: the phone never runs more than a handful of
    // concurrent requests, and a thread per core on a big.LITTLE SoC
    // costs battery for throughput nothing here can use.
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
    })
    .as_ref()
    .map_err(|e| Error::other(format!("não foi possível iniciar o runtime: {e}")))
}

/// Look up a session without holding the lock across an `.await`.
fn session_of(xuid: &str) -> Result<Arc<Session>> {
    let guard = state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.frozen.contains(xuid) {
        return Err(Error::other("conta congelada: descongele para usar"));
    }
    guard
        .sessions
        .get(xuid)
        .cloned()
        .ok_or_else(|| Error::other("conta não está conectada"))
}

fn active_xuid() -> Result<String> {
    let guard = state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .active
        .clone()
        .or_else(|| guard.first_live())
        .ok_or_else(|| Error::other("nenhuma conta conectada"))
}

// ------------------------------------------------------------ accounts

/// One account as the UI renders it.
///
/// `balance`/`unclaimed` are null until a refresh lands for this
/// account, and go back to null when one fails — a row must never show
/// a figure it did not just fetch.
fn account_json(xuid: &str, gamertag: &str, pic: &str, score: i64) -> Value {
    let email = Store::load().email_of(xuid).map(str::to_string);
    let guard = state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let farm = guard.farms.get(xuid);
    let running = farm.is_some_and(FarmHandle::is_running);
    // The row draws its own progress bar, so the cycle it is drawing has
    // to belong to that account rather than to whichever one happens to
    // be selected.
    let snap = farm.map(FarmHandle::snapshot);
    json!({
        "xuid": xuid,
        "gamertag": gamertag,
        "gamerpic": pic,
        "gamerscore": score,
        "farming": running,
        "active": guard.active.as_deref() == Some(xuid),
        "frozen": guard.frozen.contains(xuid),
        "staleReason": guard.stale.get(xuid),
        "email": email,
        "farmPercent": snap.as_ref().map(|s| s.percent),
        "farmCycle": snap.as_ref().map(|s| s.cycle),
        "farmElapsed": snap.as_ref().map(|s| s.cycle_elapsed_secs),
        "farmTotal": snap.as_ref().map(|s| s.cycle_total_secs),
        "farmState": snap.as_ref().map(|s| format!("{:?}", s.state)),
        // The last ping's outcome: accepted / denied / rejected / not_sent
        // / unknown (pending before the first). A farm Xbox refuses must
        // not look like it is earning.
        "farmPresence": snap.as_ref().map(|s| s.presence.status),
        "farmPresenceHttp": snap.as_ref().and_then(|s| s.presence.http_status),
    })
}

/// Describe every live session, merging in the balances held per xuid.
type Balances = HashMap<String, (i64, i64, Option<i64>, usize, usize, Option<String>)>;

async fn snapshot(balances: &Balances) -> Value {
    let sessions: Vec<Arc<Session>> = {
        let guard = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.live_sessions()
    };

    let mut store = Store::load();
    let mut profiles_changed = false;
    let mut out = Vec::with_capacity(sessions.len() + store.frozen.len());
    for s in sessions {
        let (pic, score) = s.profile_display().await.unwrap_or_default();
        // Kept for the day this account is frozen and may not ask.
        let cached = accounts::CachedProfile {
            gamertag: s.gamertag().to_string(),
            gamerpic: pic.clone(),
            gamerscore: score,
        };
        if store.profiles.get(s.xuid()) != Some(&cached) {
            store.profiles.insert(s.xuid().to_string(), cached);
            profiles_changed = true;
        }
        let mut row = account_json(s.xuid(), s.gamertag(), &pic, score);
        if let Some((balance, unclaimed, level, open, done, gp)) = balances.get(s.xuid()) {
            row["balance"] = json!(balance);
            row["unclaimed"] = json!(unclaimed);
            row["level"] = json!(level);
            row["missionsOpen"] = json!(open);
            row["missionsDone"] = json!(done);
            row["gamepass"] = json!(gp);
        }
        out.push(row);
    }
    // Frozen accounts: drawn from the cache alone, nothing asked of Xbox.
    for xuid in store.frozen.iter().filter(|x| store.tokens.contains_key(*x)) {
        let p = store.profiles.get(xuid).cloned().unwrap_or_else(|| accounts::CachedProfile {
            gamertag: xuid.clone(),
            ..accounts::CachedProfile::default()
        });
        let mut row = account_json(xuid, &p.gamertag, &p.gamerpic, p.gamerscore);
        // The figures from before it was frozen, flagged as such.
        if let Some(r) = store.rewards.get(xuid) {
            row["balance"] = json!(r.balance);
            row["unclaimed"] = json!(r.unclaimed);
            row["level"] = json!(r.level);
            row["cached"] = json!(true);
        }
        out.push(row);
    }
    if profiles_changed {
        if let Err(e) = store.save() {
            tracing::warn!(%e, "profile cache not saved");
        }
    }
    // Stable order, so a refresh never reshuffles the list under a
    // finger that is already reaching for a row.
    out.sort_by(|a, b| a["gamertag"].as_str().cmp(&b["gamertag"].as_str()));
    Value::Array(out)
}

/// Rebuild every stored session. One dead token costs one account, not
/// the list — its row comes back marked so the UI can offer a re-login.
async fn restore_all() -> Value {
    let mut store = Store::load();
    let mut failures = HashMap::new();
    let mut renewed = false;
    {
        let mut guard = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.frozen = store.frozen.iter().cloned().collect();
    }
    for (xuid, xauth) in store.tokens.clone() {
        // Frozen: not even the sign-in check. Its row comes from the cache.
        if store.frozen.contains(&xuid) {
            continue;
        }
        // The stored xauth first: one validation call, so a launch shows
        // the account in about a second. Only when it has died (~16h) does
        // a Windows sign-in mint a fresh one, which takes four exchanges.
        if store.windows.contains_key(&xuid) {
            if let Ok(s) = xau_mobile::session_from_token(xauth.clone()).await {
                let mut guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.sessions.insert(s.xuid().to_string(), Arc::new(s));
                continue;
            }
        }
        if let (Some(device), Some(user)) = (store.device_of(&xuid).cloned(), store.windows.get(&xuid).cloned()) {
            match xau_mobile::windows_login::restore(&device, user).await {
                Ok((s, user)) => {
                    store.put(&xuid, s.xauth());
                    store.windows.insert(xuid.clone(), user);
                    renewed = true;
                    let mut guard = state()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.sessions.insert(s.xuid().to_string(), Arc::new(s));
                    continue;
                }
                Err(e) => tracing::warn!(%e, "windows sign-in could not be renewed"),
            }
        }
        match xau_mobile::session_from_token(xauth.clone()).await {
            Ok(s) => {
                let mut guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.sessions.insert(s.xuid().to_string(), Arc::new(s));
            }
            Err(e) => {
                failures.insert(xuid.clone(), format!("sessão expirada: {e}"));
            }
        }
    }
    {
        let mut guard = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.stale.extend(failures);
        if guard.active.is_none() {
            guard.active = guard.first_live();
        }
    }
    // The renewed xauths and STS tokens; best effort, the next launch
    // renews again from whatever is on disk.
    if renewed {
        if let Err(e) = store.save() {
            tracing::warn!(%e, "renewed sign-ins not saved");
        }
    }
    snapshot(&Balances::new()).await
}

/// Read Rewards for every account, and clear the figures of any that
/// failed instead of leaving the previous ones on screen.
async fn refresh_all() -> Value {
    let sessions: Vec<Arc<Session>> = {
        let guard = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.live_sessions()
    };

    let mut balances = Balances::new();
    let mut stale = HashMap::new();
    for s in sessions {
        // Serial on purpose: these are the same Rewards endpoint under
        // one IP, and firing five at once is what a rate limiter is
        // built to notice.
        match xau_mobile::rewards(&s).await {
            Ok(info) => {
                balances.insert(
                    s.xuid().to_string(),
                    (
                        info.balance,
                        info.unclaimed,
                        info.level,
                        info.missions.iter().filter(|m| m.is_open()).count(),
                        info.missions.iter().filter(|m| m.is_done()).count(),
                        info.gamepass.clone(),
                    ),
                );
            }
            Err(e) => {
                stale.insert(s.xuid().to_string(), e.to_string());
            }
        }
    }
    {
        let mut guard = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.stale = stale;
    }
    // Keep each live read, so the card can still show it once frozen.
    let mut store = Store::load();
    let mut changed = false;
    for (xuid, (balance, unclaimed, level, ..)) in &balances {
        let r = accounts::CachedRewards { balance: *balance, unclaimed: *unclaimed, level: *level };
        if store.rewards.get(xuid) != Some(&r) {
            store.rewards.insert(xuid.clone(), r);
            changed = true;
        }
    }
    if changed {
        if let Err(e) = store.save() {
            tracing::warn!(%e, "rewards cache not saved");
        }
    }
    snapshot(&balances).await
}

/// Make a freshly signed-in session live and store it.
///
/// `windows` is the Windows sign-in behind it, when there is one: kept so
/// later launches mint a fresh xauth instead of trusting a stored one.
async fn register_session(
    session: Session,
    windows: Option<(xau_mobile::windows_login::UserSession, xau_mobile::windows_login::Device)>,
) -> Value {
    let xuid = session.xuid().to_string();

    // Register the live session BEFORE touching disk. Persisting
    // is what lets the account survive a relaunch; it is not what
    // makes the login real. Ordering these the other way round —
    // with a `?` on the save — threw away a session that had
    // authenticated perfectly well, and the screen then showed
    // "no accounts connected" as though sign-in had failed.
    let mut store = Store::load();
    store.put(&xuid, session.xauth());
    // Only sign-in ever sees this; a restored session has no
    // address, so it must be kept now or not at all.
    if let Some(email) = session.email() {
        store.put_email(&xuid, email);
    }
    match windows {
        Some((user, device)) => {
            // The Windows sign-in names the account it signed in.
            if user.username.contains('@') {
                store.put_email(&xuid, &user.username);
            }
            store.windows.insert(xuid.clone(), user);
            store.devices.insert(xuid.clone(), device);
        }
        // Signing in again the old way replaces a Windows sign-in.
        None => {
            store.windows.remove(&xuid);
            store.devices.remove(&xuid);
        }
    }
    {
        let mut guard = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.sessions.insert(xuid.clone(), Arc::new(session));
        guard.stale.remove(&xuid);
        guard.active = Some(xuid);
    }

    // Report a failed save instead of swallowing it: the account
    // works right now but will be gone after a restart, and the
    // user is entitled to know which of those two is true.
    let warning = store.save().err().map(|e| {
        tracing::warn!(%e, "account saved in memory only");
        format!("conectado, mas não foi possível salvar a sessão: {e}")
    });

    json!({
        "accounts": refresh_all().await,
        "warning": warning,
    })
}

// ------------------------------------------------------------ dispatch

#[allow(clippy::too_many_lines)] // A flat match over the wire protocol; splitting it hides the surface.
async fn dispatch(method: &str, args: &Value) -> Result<Value> {
    let arg = |k: &str| -> String {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    match method {
        // ---- sign-in ----
        "login_url" => Ok(json!({ "url": xau_mobile::login_url()? })),

        // Called on every WebView navigation. Only the real redirect is
        // terminal; anything else is still the user typing.
        "on_navigate" => Ok(match xau_mobile::on_navigate(&arg("url"))? {
            Some(code) => json!({ "done": true, "code": code }),
            None => json!({ "done": false }),
        }),

        "finish_login" => {
            let session = xau_mobile::finish_login(&arg("code")).await?;
            Ok(register_session(session, None).await)
        }

        // ---- Windows sign-in (the one the presence farm needs) ----
        "windows_login_start" => {
            let page = xau_mobile::windows_login::start().await?;
            Ok(serde_json::to_value(page)?)
        }

        // Every message the login page posts to the bridge. The reply
        // tells the WebView what to do next.
        "windows_login_message" => {
            use xau_mobile::windows_login::PageReply;
            Ok(match xau_mobile::windows_login::on_message(&arg("body")).await? {
                PageReply::Ignore => json!({ "action": "ignore" }),
                PageReply::Eval(script) => json!({ "action": "eval", "script": script }),
                PageReply::Open(url) => json!({ "action": "open", "url": url }),
                PageReply::Done(done) => {
                    let mut out = register_session(done.session, Some((done.user, done.device))).await;
                    out["action"] = json!("done");
                    out
                }
            })
        }

        "windows_login_cancel" => {
            xau_mobile::windows_login::cancel();
            Ok(json!({}))
        }

        "sign_out" => {
            let xuid = arg("xuid");
            let mut store = Store::load();
            store.remove(&xuid);
            store.save()?;
            {
                let mut guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Stop the farm first: a detached loop would keep
                // broadcasting presence for an account the user just
                // removed, with no handle left to stop it.
                if let Some(h) = guard.farms.remove(&xuid) {
                    h.stop();
                }
                guard.sessions.remove(&xuid);
                guard.stale.remove(&xuid);
                if guard.active.as_deref() == Some(xuid.as_str()) {
                    guard.active = guard.first_live();
                }
            }
            Ok(snapshot(&Balances::new()).await)
        }

        // Freeze: the account goes silent (no sign-in check, balance,
        // claim, offers or farm) and its farm stops now. Unfreeze: it
        // signs in again, the only request this makes.
        "set_frozen" => {
            let xuid = arg("xuid");
            let freeze = arg("frozen") == "1";
            let mut store = Store::load();
            if !store.tokens.contains_key(&xuid) {
                return Err(Error::other("conta não encontrada"));
            }
            if freeze {
                store.frozen.insert(xuid.clone());
            } else {
                store.frozen.remove(&xuid);
            }
            store.save()?;
            {
                let mut guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if freeze {
                    guard.frozen.insert(xuid.clone());
                    if let Some(h) = guard.farms.remove(&xuid) {
                        h.stop();
                    }
                    guard.stale.remove(&xuid);
                    if guard.active.as_deref() == Some(xuid.as_str()) {
                        guard.active = guard.first_live();
                    }
                } else {
                    guard.frozen.remove(&xuid);
                }
            }
            let needs_session = !freeze
                && !state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .sessions
                    .contains_key(&xuid);
            if needs_session {
                // Frozen since launch: it never signed in, so do it now,
                // the same way a launch would.
                let xauth = store.tokens.get(&xuid).cloned().unwrap_or_default();
                let restored = match (store.device_of(&xuid).cloned(), store.windows.get(&xuid).cloned()) {
                    (Some(device), Some(user)) => match xau_mobile::windows_login::restore(&device, user).await {
                        Ok((s, user)) => {
                            store.put(&xuid, s.xauth());
                            store.windows.insert(xuid.clone(), user);
                            let _ = store.save();
                            Ok(s)
                        }
                        Err(_) => xau_mobile::session_from_token(xauth).await,
                    },
                    _ => xau_mobile::session_from_token(xauth).await,
                };
                let mut guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match restored {
                    Ok(s) => {
                        guard.sessions.insert(xuid.clone(), Arc::new(s));
                    }
                    Err(e) => {
                        guard.stale.insert(xuid.clone(), format!("sessão expirada: {e}"));
                    }
                }
            }
            Ok(refresh_all().await)
        }

        // ---- accounts ----
        "restore" => Ok(restore_all().await),
        "refresh" => Ok(refresh_all().await),

        "select" => {
            let xuid = arg("xuid");
            let mut guard = state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.frozen.contains(&xuid) {
                return Err(Error::other("conta congelada: descongele para usar"));
            }
            if !guard.sessions.contains_key(&xuid) {
                return Err(Error::other("conta não está conectada"));
            }
            guard.active = Some(xuid);
            Ok(json!({ "ok": true }))
        }

        // One account's full mission list, for its detail screen. Read
        // live rather than cached: the screen exists to answer "what is
        // actually left", and a stale answer there is worse than a wait.
        "missions" => {
            let xuid = if arg("xuid").is_empty() {
                active_xuid()?
            } else {
                arg("xuid")
            };
            let s = session_of(&xuid)?;
            let missions = xau_mobile::missions(&s).await?;
            Ok(Value::Array(
                missions
                    .iter()
                    .map(|m| {
                        json!({
                            "id": m.activity_id,
                            "title": m.title,
                            "points": m.points,
                            "label": m.label,
                            "progress": m.progress,
                            "done": m.is_done(),
                            "open": m.is_open(),
                            "description": m.description,
                            "kind": m.kind,
                            "steps": m.steps.iter().map(|s| json!({
                                "label": s.label,
                                "points": s.points,
                                "done": s.done,
                                "note": s.note,
                                "image": s.image,
                            })).collect::<Vec<_>>(),
                            "current": m.current,
                            "count": m.count,
                            "target": m.target,
                            "claimed": m.claimed_points,
                            "locked": m.locked,
                            "requirement": m.requirement,
                            "failed": m.failed,
                            "thumbnail": m.thumbnail,
                            "resetDate": m.reset_date,
                        })
                    })
                    .collect::<Vec<_>>(),
            ))
        }

        // ---- rewards ----
        "rewards" => {
            let xuid = if arg("xuid").is_empty() {
                active_xuid()?
            } else {
                arg("xuid")
            };
            let s = session_of(&xuid)?;
            let info = xau_mobile::rewards(&s).await?;
            Ok(serde_json::to_value(info)?)
        }

        "claim" => {
            let xuid = if arg("xuid").is_empty() {
                active_xuid()?
            } else {
                arg("xuid")
            };
            let s = session_of(&xuid)?;
            let r = xau_mobile::claim(&s).await?;
            Ok(json!({
                "pointsAdded": r.points_added,
                "balance": r.balance,
                "accounts": refresh_all().await,
            }))
        }

        // Claim wherever something is actually waiting. Reads first so a
        // sweep never POSTs a claim for an account with nothing pending.
        "claim_all" => {
            let sessions: Vec<Arc<Session>> = {
                let guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.live_sessions()
            };
            let mut added = 0i64;
            let mut claimed_for = Vec::new();
            for s in sessions {
                let Ok(info) = xau_mobile::rewards(&s).await else {
                    continue;
                };
                if info.unclaimed <= 0 {
                    continue;
                }
                if let Ok(r) = xau_mobile::claim(&s).await {
                    added += r.points_added;
                    if r.points_added > 0 {
                        claimed_for.push(s.gamertag().to_string());
                    }
                }
            }
            Ok(json!({
                "pointsAdded": added,
                "gamertags": claimed_for,
                "accounts": refresh_all().await,
            }))
        }

        // ---- offers ----

        // Every account's offers at once: the screen is a cross-account
        // view, and one account failing must not blank the others.
        "offers_all" => {
            let sessions: Vec<Arc<Session>> = {
                let guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.live_sessions()
            };
            // Names come from a separate, unauthenticated content feed,
            // so it is fetched once and shared across accounts rather
            // than per account. A failure here costs labels, not data.
            let names = if let Some(first) = sessions.first() {
                xau_mobile::offer_names(first).await.unwrap_or_default()
            } else {
                Default::default()
            };
            let mut out = Vec::new();
            for s in sessions {
                let (offers, error) = match xau_mobile::offers(&s).await {
                    Ok(o) => (o, None),
                    Err(e) => (Vec::new(), Some(e.to_string())),
                };
                // Write back every claimed reward we can see.
                //
                // The CSVs used to be written only by the claim itself,
                // which made the record write-once: delete the file, or
                // claim on the Xbox app instead of here, and the code was
                // gone from this app for good even though the service
                // still returns it. Refreshing on every read makes the
                // record self-healing, and costs nothing — the same
                // bytes are rewritten over themselves.
                for o in offers.iter().filter(|o| o.resource.is_some()) {
                    let title = names.get(&o.offer_id).map(|n| n.title.as_str());
                    if let Err(e) =
                        xau_mobile::offers_csv::write_offer(s.gamertag(), s.xuid(), o, title)
                    {
                        tracing::warn!("não deu para gravar o CSV de {}: {e}", o.offer_id);
                    }
                }
                out.push(json!({
                    "xuid": s.xuid(),
                    "gamertag": s.gamertag(),
                    "error": error,
                    "offers": offers.iter().map(|o| json!({
                        "offerId": o.offer_id,
                        "title": names.get(&o.offer_id).map(|n| n.title.clone()),
                        "description": names.get(&o.offer_id).map(|n| n.description.clone()),
                        "status": o.offer_status,
                        "resourceType": o.resource_type,
                        "resource": o.resource,
                        "claimedDate": o.claimed_date,
                        "available": o.is_available(),
                        "claimed": o.is_claimed(),
                    })).collect::<Vec<_>>(),
                }));
            }
            Ok(Value::Array(out))
        }

        // Claim every available offer, on every account.
        //
        // IRREVERSIBLE, and only ever reached from an explicit tap —
        // never a poll, never the auto-claim that settles points. Each
        // reward is written to its own CSV BEFORE being reported, so a
        // resource that is minted once cannot be lost to a crash or a
        // dismissed toast.
        // Every code recorded for one offer, across all accounts. Read
        // from the CSVs on disk, so an account that claimed on an
        // earlier run -- or has since been signed out -- still shows up.
        "offer_codes" => {
            let rows = xau_mobile::offers_csv::rows_for_offer(&arg("offerId"))?;
            Ok(Value::Array(
                rows.iter()
                    .map(|r| {
                        json!({
                            "gamertag": r.gamertag,
                            "code": r.code,
                            "claimedDate": r.claimed_date,
                        })
                    })
                    .collect::<Vec<_>>(),
            ))
        }

        // The same rows as a file the user can keep. The text crosses
        // the bridge rather than a path: the CSVs live in the app's
        // private directory, which nothing else on the phone can open,
        // so the download is a copy written where the user chooses.
        "offer_csv" => {
            // An empty id matches the suffix `_.csv`, which matches
            // nothing, and would hand back a header-only file as a
            // success — indistinguishable from "nothing claimed yet".
            let offer_id = arg("offerId");
            if offer_id.is_empty() || !offer_id.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(Error::other("vantagem inválida"));
            }
            let csv = xau_mobile::offers_csv::csv_for_offer(&offer_id, &arg("title"))?;
            Ok(json!({ "fileName": csv.file_name, "text": csv.text }))
        }

        // `offerId` narrows it to one reward, claimed on every account
        // that still has it: what a tap on that reward's own screen
        // means. Absent or empty, every available offer is claimed.
        "claim_offers_all" => {
            let only = arg("offerId");
            let sessions: Vec<Arc<Session>> = {
                let guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.live_sessions()
            };
            let names = if let Some(first) = sessions.first() {
                xau_mobile::offer_names(first).await.unwrap_or_default()
            } else {
                Default::default()
            };
            let mut claimed = Vec::new();
            let mut failures = Vec::new();
            for s in sessions {
                let available = match xau_mobile::offers(&s).await {
                    Ok(list) => list,
                    Err(e) => {
                        failures.push(json!({ "gamertag": s.gamertag(), "error": e.to_string() }));
                        continue;
                    }
                };
                for offer in available
                    .iter()
                    .filter(|o| o.is_available())
                    .filter(|o| only.is_empty() || o.offer_id == only)
                {
                    // Serial: these are claims against one account under
                    // one IP, and a burst is what a rate limiter notices.
                    match xau_mobile::claim_offer(&s, &offer.offer_id).await {
                        Ok(done) => {
                            let csv = xau_mobile::offers_csv::write_offer(
                                s.gamertag(),
                                s.xuid(),
                                &done,
                                names.get(&done.offer_id).map(|n| n.title.as_str()),
                            );
                            claimed.push(json!({
                                "gamertag": s.gamertag(),
                                "offerId": done.offer_id,
                                "title": names.get(&done.offer_id).map(|n| n.title.clone()),
                                "resourceType": done.resource_type,
                                "resource": done.resource,
                                "csv": csv.as_ref().ok().map(|p| p.display().to_string()),
                                "csvError": csv.err().map(|e| e.to_string()),
                            }));
                        }
                        Err(e) => failures.push(json!({
                            "gamertag": s.gamertag(),
                            "offerId": offer.offer_id,
                            "error": e.to_string(),
                        })),
                    }
                }
            }
            let index = xau_mobile::offers_csv::write_index()
                .ok()
                .map(|p| p.display().to_string());
            Ok(json!({
                "claimed": claimed,
                "failures": failures,
                "indexCsv": index,
            }))
        }

        // ---- farm ----
        // ---- Microsoft Store catalogue (public; no account needed) ----
        "store_search" => {
            let cursor = arg("cursor");
            let page = xau_mobile::store::search(&arg("query"), Some(cursor.as_str())).await?;
            Ok(serde_json::to_value(page)?)
        }
        // Search and list in one call, filtered here: the phone gets only
        // the games that pass, and the pages stay cached for the next filter.
        "store_browse" => {
            let cursor = arg("cursor");
            let filter = xau_mobile::store::Filter {
                microsoft_only: arg("microsoft") == "1",
                hide_free: arg("hideFree") == "1",
                min: arg("min").parse().ok(),
                max: arg("max").parse().ok(),
            };
            let page = xau_mobile::store::browse(
                &arg("query"),
                &arg("list"),
                Some(cursor.as_str()),
                &filter,
                arg("want").parse().unwrap_or(36),
            )
            .await?;
            Ok(serde_json::to_value(page)?)
        }
        "store_prewarm" => {
            xau_mobile::store::prewarm().await;
            Ok(json!({}))
        }
        "store_featured" => {
            let cursor = arg("cursor");
            let page = xau_mobile::store::featured(&arg("list"), Some(cursor.as_str())).await?;
            Ok(serde_json::to_value(page)?)
        }
        "store_purchase_url" => Ok(json!({ "url": xau_mobile::store::purchase_url(&arg("productId")) })),
        // The checkout, signed in the way the Xbox app does it: an XSTS
        // for mp.microsoft.com posted as `xToken`, no web session needed.
        "store_purchase_form" => {
            let xuid = active_xuid()?;
            if state().lock().unwrap_or_else(std::sync::PoisonError::into_inner).frozen.contains(&xuid) {
                return Err(Error::other("conta congelada: descongele para usar"));
            }
            let mut store = Store::load();
            let (Some(device), Some(mut user)) =
                (store.device_of(&xuid).cloned(), store.windows.get(&xuid).cloned())
            else {
                return Err(Error::other("entre de novo com o login do Windows para comprar"));
            };
            let token = xau_mobile::windows_login::issue_store_token(&device, &mut user).await?;
            store.windows.insert(xuid, user);
            if let Err(e) = store.save() {
                tracing::warn!(%e, "renewed sign-in not saved");
            }
            let form = xau_mobile::store::purchase_form(&arg("productId"), &token).await?;
            Ok(serde_json::to_value(form)?)
        }

        // Every farm this process has run, read from memory only: the
        // foreground service polls this every few seconds for its
        // notification, and must not cost a request each time.
        "farm_summary" => {
            let guard = state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(Value::Array(
                guard
                    .farms
                    .iter()
                    .map(|(xuid, h)| {
                        let s = h.snapshot();
                        json!({
                            "xuid": xuid,
                            "gamertag": guard.sessions.get(xuid).map(|s| s.gamertag().to_string()),
                            "running": h.is_running(),
                            "elapsed": s.cycle_elapsed_secs,
                            "total": s.cycle_total_secs,
                            "presence": s.presence.status,
                        })
                    })
                    .collect(),
            ))
        }

        // With box art when an account is signed in to fetch it with;
        // `image` is null otherwise, and the screen falls back to initials.
        "farm_titles" => {
            let art = match active_xuid().and_then(|x| session_of(&x)) {
                Ok(s) => xau_mobile::farm_title_art(&s).await,
                Err(_) => HashMap::new(),
            };
            Ok(Value::Array(
                xau_mobile::farm_titles()
                    .into_iter()
                    .map(|t| json!({ "titleId": t.id, "name": t.name, "image": art.get(&t.id) }))
                    .collect(),
            ))
        }

        // "Trocar jogos": the next pool of eight Microsoft games. A farm
        // already running keeps its games; the next start uses these.
        "farm_next_pool" => {
            let (index, _) = xau_mobile::farm_titles::next()?;
            Ok(json!({ "pool": index + 1, "pools": xau_mobile::farm_titles::POOLS }))
        }
        "farm_pool" => Ok(json!({
            "pool": xau_mobile::farm_titles::current_pool() + 1,
            "pools": xau_mobile::farm_titles::POOLS,
        })),

        "farm_start" => {
            let xuid = if arg("xuid").is_empty() {
                active_xuid()?
            } else {
                arg("xuid")
            };
            let s = session_of(&xuid)?;
            // Starting twice would leave the first loop detached and
            // unstoppable, doubling the presence traffic.
            {
                let guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if guard.farms.get(&xuid).is_some_and(FarmHandle::is_running) {
                    return Ok(json!({ "running": true }));
                }
            }
            let handle = xau_mobile::start_farm(&s)?;
            let mut guard = state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.farms.insert(xuid, handle);
            Ok(json!({ "running": true }))
        }

        "farm_stop" => {
            let xuid = if arg("xuid").is_empty() {
                active_xuid()?
            } else {
                arg("xuid")
            };
            let mut guard = state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(h) = guard.farms.remove(&xuid) {
                h.stop();
            }
            Ok(json!({ "running": false }))
        }

        "farm_state" => {
            let xuid = if arg("xuid").is_empty() {
                active_xuid()?
            } else {
                arg("xuid")
            };
            let guard = state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match guard.farms.get(&xuid) {
                Some(h) => Ok(serde_json::to_value(h.snapshot())?),
                None => Ok(json!({ "state": "Idle", "percent": 0.0, "cycle": 0 })),
            }
        }

        // ---- misc ----
        "public_ip" => Ok(json!({ "ip": accounts::public_ip().await? })),

        other => Err(Error::other(format!("método desconhecido: {other}"))),
    }
}

// --------------------------------------------------------------- entry

/// Initialise logging and tell the core where it may write.
///
/// `data_dir` must be the app's own `Context.getFilesDir()`. Rust cannot
/// work this out for itself on Android — the `directories` crate falls
/// back to `$HOME/.local/share`, which for an app process is `/` and is
/// not writable. Getting it from Java is the only correct answer.
///
/// `no_mangle` is what makes the symbol findable by name from the JVM;
/// the workspace warns on it because a duplicate export would be
/// undefined behaviour. Both symbols here are namespaced by the JNI
/// convention (`Java_<package>_<class>_<method>`), which no other
/// library in this process can collide with.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "system" fn Java_com_xau_rewards_core_Native_init(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
) {
    #[cfg(target_os = "android")]
    {
        // android_logger consumes `log` records. `tracing`'s "log"
        // feature re-emits every tracing event as one — but ONLY while
        // no tracing subscriber is installed. Installing a
        // tracing_subscriber registry here (which had no output layer
        // and so printed nothing itself) silently switched that bridge
        // off, and the app traced into a void.
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("xau"),
        );
    }

    // Read it unconditionally so the host contract is the same on every
    // target and a desktop test harness can point the store somewhere
    // temporary. An empty string means "you decide", which keeps the
    // desktop path working.
    if let Ok(dir) = env.get_string(&data_dir) {
        let dir: String = dir.into();
        if !dir.is_empty() {
            tracing::info!(%dir, "account store directory");
            accounts::set_data_dir(dir);
        }
    }
}

/// The single crossing point. See the module docs for why there is one.
#[allow(unsafe_code)] // JNI-namespaced export; see `Native_init` above.
#[no_mangle]
pub extern "system" fn Java_com_xau_rewards_core_Native_call(
    mut env: JNIEnv,
    _class: JClass,
    method: JString,
    args: JString,
) -> jstring {
    // A panic unwinding into the JVM is undefined behaviour, so nothing
    // below may escape. `AssertUnwindSafe` is honest here: the state a
    // panic could leave inconsistent is the session map, and a poisoned
    // lock is recovered explicitly everywhere it is taken.
    let out = catch_unwind(AssertUnwindSafe(|| {
        let method: String = env
            .get_string(&method)
            .map_err(|e| Error::other(e.to_string()))?
            .into();
        let args: String = env
            .get_string(&args)
            .map_err(|e| Error::other(e.to_string()))?
            .into();
        let args: Value = serde_json::from_str(&args).unwrap_or(Value::Null);
        runtime()?.block_on(dispatch(&method, &args))
    }));

    let body = match out {
        Ok(Ok(v)) => json!({ "ok": v }),
        Ok(Err(e)) => json!({ "err": e.to_string() }),
        Err(_) => json!({ "err": "falha interna inesperada" }),
    };

    // If even the response string cannot be built there is nothing left
    // to report with, so hand back null and let Kotlin raise.
    env.new_string(body.to_string())
        .map_or(std::ptr::null_mut(), JString::into_raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unknown method must fail as data, not by panicking across the
    /// FFI boundary where a panic is undefined behaviour.
    #[test]
    fn an_unknown_method_is_an_error_not_a_panic() {
        let rt = runtime().expect("runtime");
        let r = rt.block_on(dispatch("does_not_exist", &Value::Null));
        assert!(r.is_err_and(|e| e.to_string().contains("desconhecido")));
    }

    /// `login_url` must not need a session — it is what the user calls
    /// before they have one.
    #[test]
    fn the_login_url_is_available_before_any_account_exists() {
        let rt = runtime().expect("runtime");
        let v = rt
            .block_on(dispatch("login_url", &Value::Null))
            .expect("url");
        let url = v["url"].as_str().unwrap_or_default();
        assert!(
            url.starts_with("https://login.live.com/oauth20_authorize.srf"),
            "{url}"
        );
    }

    /// Acting on an account that was never added must be refused rather
    /// than silently retargeted at whatever session happens to be first.
    #[test]
    fn selecting_an_unknown_account_is_refused() {
        let rt = runtime().expect("runtime");
        let r = rt.block_on(dispatch("select", &json!({ "xuid": "999" })));
        assert!(r.is_err());
    }

    /// Farm state for an account with no farm is a real answer, not an
    /// error — the UI asks for it on every account, every tick.
    #[test]
    fn farm_state_without_a_farm_reports_idle() {
        let rt = runtime().expect("runtime");
        let v = rt
            .block_on(dispatch("farm_state", &json!({ "xuid": "1" })))
            .expect("idle");
        assert_eq!(v["state"], "Idle");
    }
}
