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

use std::collections::HashMap;
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
        .or_else(|| guard.sessions.keys().next().cloned())
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
        "staleReason": guard.stale.get(xuid),
        "email": email,
        "farmPercent": snap.as_ref().map(|s| s.percent),
        "farmCycle": snap.as_ref().map(|s| s.cycle),
        "farmElapsed": snap.as_ref().map(|s| s.cycle_elapsed_secs),
        "farmTotal": snap.as_ref().map(|s| s.cycle_total_secs),
        "farmState": snap.as_ref().map(|s| format!("{:?}", s.state)),
    })
}

/// Describe every live session, merging in the balances held per xuid.
type Balances = HashMap<String, (i64, i64, Option<i64>, usize, usize, Option<String>)>;

async fn snapshot(balances: &Balances) -> Value {
    let sessions: Vec<Arc<Session>> = {
        let guard = state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.sessions.values().cloned().collect()
    };

    let mut out = Vec::with_capacity(sessions.len());
    for s in sessions {
        let (pic, score) = s.profile_display().await.unwrap_or_default();
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
    // Stable order, so a refresh never reshuffles the list under a
    // finger that is already reaching for a row.
    out.sort_by(|a, b| a["gamertag"].as_str().cmp(&b["gamertag"].as_str()));
    Value::Array(out)
}

/// Rebuild every stored session. One dead token costs one account, not
/// the list — its row comes back marked so the UI can offer a re-login.
async fn restore_all() -> Value {
    let store = Store::load();
    let mut failures = HashMap::new();
    for (xuid, xauth) in &store.tokens {
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
            guard.active = guard.sessions.keys().next().cloned();
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
        guard.sessions.values().cloned().collect()
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
    snapshot(&balances).await
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

            Ok(json!({
                "accounts": refresh_all().await,
                "warning": warning,
            }))
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
                    guard.active = guard.sessions.keys().next().cloned();
                }
            }
            Ok(snapshot(&Balances::new()).await)
        }

        // ---- accounts ----
        "restore" => Ok(restore_all().await),
        "refresh" => Ok(refresh_all().await),

        "select" => {
            let xuid = arg("xuid");
            let mut guard = state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let info = xau_mobile::rewards(&s).await?;
            Ok(Value::Array(
                info.missions
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
                guard.sessions.values().cloned().collect()
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
                guard.sessions.values().cloned().collect()
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
        "claim_offers_all" => {
            let sessions: Vec<Arc<Session>> = {
                let guard = state()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.sessions.values().cloned().collect()
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
                for offer in available.iter().filter(|o| o.is_available()) {
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
        "farm_titles" => Ok(Value::Array(
            xau_mobile::farm_titles()
                .into_iter()
                .map(|(id, name)| json!({ "titleId": id, "name": name }))
                .collect(),
        )),

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
