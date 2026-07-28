//! "NUCLEAR BRUTE FORCE" runner — unlock 100% of every game the
//! **caller's own** profile has ever touched.
//!
//! Unlike the replay runner (which is bounded by what a *target*
//! profile has unlocked), nuclear walks the caller's own games list,
//! fetches each title's full achievement catalogue, filters the
//! not-yet-unlocked entries, and POSTs them.
//!
//! # Pipeline
//!
//! 1. `GET titlehub/.../titleHistory` — caller's games list.
//! 2. Filter replayable (PC / XboxOne / XboxSeries / Cloud / Win32).
//! 3. For each title, bounded by semaphore:
//!    - `GET achievements.xboxlive.com/.../achievements?titleId=X` for the catalogue
//!    - Filter `progressState != "Achieved"` — the pending ones
//!    - `POST .../achievements/{scid}/update` chunked at `UNLOCK_CHUNK_SIZE` (or 1 when strict-order)
//! 4. Progress visible via [`NuclearHandle::snapshot`].
//!
//! # Rate-limit awareness
//! The update endpoint is 100 req / 15s. Default `title_concurrency=6`
//! keeps us inside that budget. The per-endpoint throttle gate from
//! `xbox::client` absorbs accidental 429s.
//!
//! # Destructiveness
//! This fires a large burst of unlock notifications + gamerscore on
//! the caller's profile. Every UI that exposes it MUST triple-confirm.

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::error::{Error, Result};
use crate::plan::builder::{enumerate_titles_from_bulk_scan, is_title_replayable};
use crate::xbox::XboxClient;

/// Tunable parameters for a nuclear run.
#[derive(Debug, Clone)]
pub struct NuclearConfig {
    /// Caller's own xuid. All unlocks go to this profile.
    pub my_xuid: String,
    /// Titles unlocked in parallel. Default 6 — matches the sustained
    /// RPS ceiling on the update endpoint (100 req / 15s).
    pub title_concurrency: usize,
    /// Override `UNLOCK_CHUNK_SIZE` for the per-title unlock POSTs.
    /// `Some(1)` = strict per-game chronological order (one POST per
    /// ach → monotonic `timeUnlocked`). `None` = server max 64.
    pub chunk_size_override: Option<usize>,
    /// Cap total titles processed (`0` = no cap). Useful for "try this
    /// on 10 titles first before committing".
    pub max_titles: usize,
    /// Include titles whose `currentAchievements == totalAchievements`.
    /// Default `false` because the GET+POST round-trips are wasted on
    /// fully-complete titles. Flip to `true` if Xbox lost progress state
    /// for some titles and you want to re-push.
    pub include_already_complete: bool,
    /// Where the list of titles comes from.
    ///
    /// - `TitleSource::TitleHub` (fast, ~1s): enumerates via Xbox's
    ///   games list endpoint. Returns ONLY titles with gameplay
    ///   presence (the user actually launched them). Profiles that
    ///   unlocked achievements via replay/nuclear **will miss** any
    ///   title they never played — which is catastrophic for a
    ///   "nuclear brute force" operation that's supposed to hit
    ///   everything.
    /// - `TitleSource::BulkScan` (slow, 10-60s): enumerates via the
    ///   bulk achievements endpoint, pages through every achievement
    ///   on the profile, aggregates per-title. Returns every title
    ///   that has at least one achievement tracked server-side.
    ///   This is the complete source.
    ///
    /// Default is `BulkScan` — correctness beats speed on a one-shot
    /// destructive operation.
    pub title_source: TitleSource,
    /// Unlock-safety policy. Default strict — revoked + time-window
    /// violations are never unlocked. Opt out only via explicit
    /// `--allow-unobtainable` override.
    pub safety: crate::plan::safety::SafetyPolicy,
}

/// Strategy for enumerating the titles a nuclear run will process.
/// See `NuclearConfig::title_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// `titlehub/titleHistory` — fast but only returns games with
    /// gameplay presence.
    TitleHub,
    /// Bulk `achievements?unlockedOnly=false` scan — complete but
    /// slow. The ONLY way to see titles that exist on the profile
    /// without the user having played them (e.g. replayed titles).
    BulkScan,
}

impl Default for NuclearConfig {
    fn default() -> Self {
        Self {
            my_xuid: String::new(),
            title_concurrency: 6,
            chunk_size_override: None,
            max_titles: 0,
            include_already_complete: false,
            title_source: TitleSource::BulkScan,
            safety: crate::plan::safety::SafetyPolicy::strict(),
        }
    }
}

/// Observable state of a nuclear run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NuclearProgress {
    pub state: NuclearState,
    pub total_titles: usize,
    pub done_titles: usize,
    pub unlocked_achievements: usize,
    pub gamerscore_earned: i32,
    /// Titles event-based without a matching bundle template → had to
    /// be skipped (unlock endpoint would return `code:49`).
    pub skipped_event_based: usize,
    /// Titles already at 100% at enumeration time → nothing to do.
    pub skipped_already_complete: usize,
    /// Titles that returned a hard error from Xbox (HTTP 4xx/5xx
    /// other than the idempotent 304 / event-based 49).
    pub errored_titles: usize,
    pub last_title: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NuclearState {
    #[default]
    Idle,
    Enumerating,
    Running,
    Done,
    Stopped,
    Error,
}

/// Stable handle to a running nuclear task. `clone()` to share between
/// the progress-renderer and the stop-button.
#[derive(Clone)]
pub struct NuclearHandle {
    shared: Arc<RwLock<NuclearProgress>>,
    stop_tx: Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl NuclearHandle {
    pub fn snapshot(&self) -> NuclearProgress {
        self.shared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Signal the runner to stop between title completions. Returns
    /// `true` if the signal fired, `false` if the runner had already
    /// exited before the stop was requested.
    pub fn stop(&self) -> bool {
        if let Some(tx) = self
            .stop_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = tx.send(());
            true
        } else {
            false
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.shared
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .state,
            NuclearState::Enumerating | NuclearState::Running
        )
    }
}

/// Candidate title for processing — carries just the fields we need
/// across the task boundary. `Send + 'static` so spawn_blocking-style
/// closures can own one.
#[derive(Debug, Clone)]
struct TitleCandidate {
    title_id: String,
    name: String,
    current_achievements: i32,
    total_achievements: i32,
}

/// Per-title outcome collected by the drain loop.
#[derive(Debug)]
struct TitleOutcome {
    /// Numeric title_id for forensic log filtering (`unlock-history.jsonl`
    /// `kind=nuclear` lines). Allows multi-profile users to
    /// `jq 'select(.title_id == "...")'` after a run.
    title_id: String,
    name: String,
    result: TitleResult,
}

#[derive(Debug)]
enum TitleResult {
    Unlocked {
        count: usize,
        gamerscore: i32,
    },
    /// Event-based title where some IDs landed and some didn't.
    /// Distinct from `Unlocked` so the drain loop can credit the
    /// successes AND surface a user-visible warning for the
    /// failures instead of silently clearing `last_error`.
    Partial {
        count: usize,
        gamerscore: i32,
        failed: usize,
        message: String,
    },
    AlreadyComplete,
    EventBasedSkipped,
    Error(String),
}

/// Kick off a nuclear run. Returns immediately with a handle; the
/// work happens in a detached tokio task.
pub fn start_nuclear(client: XboxClient, config: NuclearConfig) -> Result<NuclearHandle> {
    if config.my_xuid.trim().is_empty() {
        return Err(Error::invalid_argument("my_xuid is required"));
    }
    // BUG HUNTER runner-lock (#12): per-xuid exclusion across processes.
    let runner_lock = crate::plan::runner_lock::RunnerLock::try_acquire(&config.my_xuid)?;

    let progress = NuclearProgress {
        state: NuclearState::Enumerating,
        ..Default::default()
    };
    let shared = Arc::new(RwLock::new(progress));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let stop_holder = Arc::new(std::sync::Mutex::new(Some(stop_tx)));
    let handle = NuclearHandle {
        shared: shared.clone(),
        stop_tx: stop_holder,
    };

    // Wave-6 audit state-F2: same bug as the replay supervisor —
    // unsupervised spawn meant a panic anywhere inside `run()`
    // (e.g., a divide-by-zero in chunk-size math, an unwrap on a
    // borked plan entry) would drop the task while
    // `state == Enumerating | Running` was still committed. The
    // IPC's "nuclear is running; stop it before retry" guard then
    // rejected every subsequent run forever. Mirror the bit_by_bit
    // / replay supervisor pattern: wrap the work in an inner
    // spawn, observe the JoinError, and force-flip state to Error
    // on any abnormal exit.
    let shared_for_recovery = shared.clone();
    tokio::spawn(async move {
        let _lock = runner_lock;
        let inner = tokio::spawn(async move {
            run(client, config, shared, stop_rx).await;
        });
        match inner.await {
            Ok(()) => {}
            Err(je) => {
                let reason = if je.is_panic() {
                    format!("nuclear run panicked — task supervised: {je}")
                } else if je.is_cancelled() {
                    "nuclear run cancelled unexpectedly (runtime shutdown?)".to_string()
                } else {
                    format!("nuclear run terminated abnormally: {je}")
                };
                tracing::error!(
                    ?je,
                    "nuclear run() ended abnormally — flipping state to Error"
                );
                let mut w = shared_for_recovery
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match w.state {
                    NuclearState::Done | NuclearState::Stopped | NuclearState::Error => {}
                    _ => {
                        w.state = NuclearState::Error;
                        if w.last_error.is_none() {
                            w.last_error = Some(reason);
                        }
                    }
                }
            }
        }
    });
    Ok(handle)
}

async fn run(
    client: XboxClient,
    config: NuclearConfig,
    shared: Arc<RwLock<NuclearProgress>>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    // 1) Enumerate titles. Two sources — see `TitleSource` docs. BulkScan
    //    (default) uses the bulk achievements endpoint and returns
    //    **every** title with achievements tracked on the profile,
    //    including replayed-only games. TitleHub is the fast fallback.
    let candidates: Vec<TitleCandidate> = match config.title_source {
        TitleSource::TitleHub => {
            let games = match client.get_games_list(&config.my_xuid).await {
                Ok(g) => g,
                Err(e) => {
                    let mut w = shared
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    w.state = NuclearState::Error;
                    w.last_error = Some(format!("games list: {e}"));
                    return;
                }
            };
            games
                .titles
                .into_iter()
                .filter_map(|t| {
                    let title_id = t.title_id.clone()?;
                    let name = t.name.clone().unwrap_or_default();
                    if !is_title_replayable(&t.devices) {
                        return None;
                    }
                    let ach = t.achievement.unwrap_or_default();
                    if !config.include_already_complete
                        && ach.total_achievements > 0
                        && ach.current_achievements >= ach.total_achievements
                    {
                        return None;
                    }
                    Some(TitleCandidate {
                        title_id,
                        name,
                        current_achievements: ach.current_achievements,
                        total_achievements: ach.total_achievements,
                    })
                })
                .collect()
        }
        TitleSource::BulkScan => {
            // Enumerate via the bulk achievements endpoint — the
            // complete picture. No `devices` field here so we can't
            // filter X360/mobile-only; assume replayable. Xbox only
            // tracks modern achievements via this endpoint anyway, so
            // 360-only titles don't appear.
            let enumerated = match enumerate_titles_from_bulk_scan(&client, &config.my_xuid).await {
                Ok(t) => t,
                Err(e) => {
                    let mut w = shared
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    w.state = NuclearState::Error;
                    w.last_error = Some(format!("bulk enumerate: {e}"));
                    return;
                }
            };
            enumerated
                .into_iter()
                .filter(|t| {
                    config.include_already_complete
                        || !(t.total_achievements > 0
                            && t.current_achievements >= t.total_achievements)
                })
                .map(|t| TitleCandidate {
                    title_id: t.title_id,
                    name: t.name,
                    current_achievements: t.current_achievements,
                    total_achievements: t.total_achievements,
                })
                .collect()
        }
    };

    // BBB-Discontinued: drop user-blocklisted titles BEFORE the
    // per-title fan-out spawns. Same blocklist source as
    // bit_by_bit / replay; nuclear is the third unlock-producing
    // entry-point and has to honor the same contract. Audit each
    // skipped title to `unlock-safety-skipped.jsonl`.
    let blocklist = crate::plan::discontinued::load_discontinued_set();
    let pre_block_len = candidates.len();
    let candidates: Vec<TitleCandidate> = candidates
        .into_iter()
        .filter(|c| {
            if crate::plan::discontinued::is_discontinued(&blocklist, &c.name) {
                tracing::info!(
                    title_id = %c.title_id,
                    game_name = %c.name,
                    "nuclear: discontinued title skipped (BBB-Discontinued)"
                );
                crate::plan::safety::append_skip_audit(&crate::plan::safety::SkipAuditEntry {
                    iso_timestamp: crate::plan::runner::iso_now_simple_public(),
                    reason: "discontinued blocklist match — filtered upstream of nuclear unlock"
                        .into(),
                    title_id: c.title_id.clone(),
                    title_name: c.name.clone(),
                    achievement_id: String::new(),
                    achievement_name: String::new(),
                    gamerscore: 0,
                    classification: crate::plan::safety::SafetyClass::Discontinued,
                    xuid: config.my_xuid.clone(),
                });
                false
            } else {
                true
            }
        })
        .collect();
    let blocked_count = pre_block_len.saturating_sub(candidates.len());
    if blocked_count > 0 {
        tracing::warn!(
            blocked = blocked_count,
            remaining = candidates.len(),
            "nuclear: discontinued blocklist removed {} of {} candidate titles",
            blocked_count,
            pre_block_len
        );
    }
    if candidates.is_empty() {
        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.state = NuclearState::Error;
        w.last_error = Some(
            "every candidate title is on the discontinued blocklist — \
             nothing to unlock. Edit `Documents/XAU/discontinued.txt` if you want to override."
                .into(),
        );
        return;
    }

    let titles_total = if config.max_titles > 0 {
        candidates.len().min(config.max_titles)
    } else {
        candidates.len()
    };
    {
        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.state = NuclearState::Running;
        w.total_titles = titles_total;
    }

    // 3) Fan out per-title unlock tasks, bounded by semaphore.
    let sem = Arc::new(tokio::sync::Semaphore::new(config.title_concurrency.max(1)));
    let mut joins: tokio::task::JoinSet<TitleOutcome> = tokio::task::JoinSet::new();
    let chunk_size = config.chunk_size_override;

    for cand in candidates.into_iter().take(titles_total) {
        if stop_rx.try_recv().is_ok() {
            shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .state = NuclearState::Stopped;
            return;
        }
        let Ok(permit) = sem.clone().acquire_owned().await else {
            break;
        };
        let client = client.clone();
        let my_xuid = config.my_xuid.clone();
        let config_cl = config.clone();
        joins.spawn(async move {
            let _permit = permit;
            process_title(&client, &my_xuid, cand, chunk_size, &config_cl).await
        });
    }

    // 4) Drain completions.
    while let Some(joined) = joins.join_next().await {
        if stop_rx.try_recv().is_ok() {
            shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .state = NuclearState::Stopped;
            return;
        }
        match joined {
            Ok(outcome) => {
                // BUG HUNTER M-nuclear-history-line: nuclear runner
                // previously emitted nothing to `unlock-history.jsonl`,
                // leaving forensic auditors blind to what landed
                // (replay/bulk and BBB both write detailed lines per
                // achievement). Emit a title-level summary line at
                // each `Ok(outcome)` so multi-profile users can
                // reconstruct nuclear runs by xuid filter (matches
                // the BBB / SkipAuditEntry forensic model from
                // iter#11/12). Title-level (not per-ach) granularity
                // is acceptable because `TitleOutcome` doesn't carry
                // per-id results; if needed in future, the spawn
                // closure can collect ids and emit per-ach lines.
                let (status_str, extra) = match &outcome.result {
                    TitleResult::Unlocked { count, gamerscore } => (
                        "success",
                        serde_json::json!({"count": count, "gamerscore": gamerscore}),
                    ),
                    TitleResult::Partial {
                        count,
                        gamerscore,
                        failed,
                        message,
                    } => (
                        "partial",
                        serde_json::json!({
                            "count": count,
                            "gamerscore": gamerscore,
                            "failed": failed,
                            "error": message,
                        }),
                    ),
                    TitleResult::AlreadyComplete => ("already_complete", serde_json::json!({})),
                    TitleResult::EventBasedSkipped => (
                        "skipped",
                        serde_json::json!({"reason": "event_based_not_in_bundle"}),
                    ),
                    TitleResult::Error(msg) => ("failed", serde_json::json!({"error": msg})),
                };
                let history_line = serde_json::json!({
                    "kind": "nuclear",
                    "ts": crate::plan::runner::iso_now_simple_public(),
                    "xuid": config.my_xuid,
                    "title_id": outcome.title_id,
                    "game": outcome.name,
                    "status": status_str,
                    "extra": extra,
                })
                .to_string();
                let _ = crate::plan::runner::append_history_line(&history_line);
                let mut w = shared
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                w.done_titles = w.done_titles.saturating_add(1);
                w.last_title = Some(outcome.name.clone());
                match outcome.result {
                    TitleResult::Unlocked { count, gamerscore } => {
                        w.unlocked_achievements = w.unlocked_achievements.saturating_add(count);
                        w.gamerscore_earned = w.gamerscore_earned.saturating_add(gamerscore);
                        w.last_error = None;
                    }
                    TitleResult::Partial {
                        count,
                        gamerscore,
                        failed: _,
                        message,
                    } => {
                        // Credit the REAL successes (count + summed
                        // gamerscore from the ids that landed) and
                        // increment errored_titles by 1 — this is
                        // ONE title that had failures, regardless of
                        // how many achievements inside it failed.
                        // `last_error` stays populated so the HUD
                        // shows the user what happened.
                        w.unlocked_achievements = w.unlocked_achievements.saturating_add(count);
                        w.gamerscore_earned = w.gamerscore_earned.saturating_add(gamerscore);
                        w.errored_titles = w.errored_titles.saturating_add(1);
                        w.last_error = Some(format!("{}: {message}", outcome.name));
                    }
                    TitleResult::AlreadyComplete => {
                        w.skipped_already_complete = w.skipped_already_complete.saturating_add(1);
                    }
                    TitleResult::EventBasedSkipped => {
                        w.skipped_event_based = w.skipped_event_based.saturating_add(1);
                        w.last_error =
                            Some(format!("{} — event-based (not in bundle)", outcome.name));
                    }
                    TitleResult::Error(msg) => {
                        w.errored_titles = w.errored_titles.saturating_add(1);
                        w.last_error = Some(format!("{}: {msg}", outcome.name));
                    }
                }
            }
            Err(e) => {
                // Wave-14 audit sweep #1 (MED): the bit_by_bit twin
                // (`bit_by_bit.rs::run` Err(JoinError) arm) bumps
                // `errored_titles` and writes `last_error` so the
                // GUI snapshot reflects the panic. Nuclear used to
                // log a tracing warn and otherwise stay silent —
                // a panicked `process_title` would leave the
                // progress snapshot unchanged, the user would see
                // "Done" with everything-zeros after a panic, and
                // there'd be no forensic indicator of the fault.
                // Mirror the bit_by_bit pattern.
                tracing::warn!(?e, "nuclear title task panicked");
                let mut w = shared
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                w.errored_titles = w.errored_titles.saturating_add(1);
                w.last_error = Some(format!("title task panicked: {e}"));
            }
        }
    }

    shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .state = NuclearState::Done;
}

#[allow(clippy::items_after_statements)]
async fn process_title(
    client: &XboxClient,
    my_xuid: &str,
    cand: TitleCandidate,
    chunk_size_override: Option<usize>,
    config: &NuclearConfig,
) -> TitleOutcome {
    // `use` inside fn body is intentional: the safety gate is a
    // big-enough block to earn its own local `use` clause for
    // readability. Clippy's `items_after_statements` lint
    // disagrees — suppressed fn-locally.
    let name = cand.name.clone();
    // BUG HUNTER M-nuclear-history-line: capture title_id once so
    // every `TitleOutcome` construction site (10+ exit paths) can
    // include it for the forensic log line emitted in the drain
    // loop. Without this, the audit log can't filter nuclear
    // outcomes by title.
    let title_id = cand.title_id.clone();

    // Fast path: already 100% at enumeration time.
    if cand.total_achievements > 0 && cand.current_achievements >= cand.total_achievements {
        return TitleOutcome {
            title_id: title_id.clone(),
            name,
            result: TitleResult::AlreadyComplete,
        };
    }

    // Fetch catalogue — needs scan-endpoint GET. Cached in the client.
    let catalogue = match client
        .get_achievements_for_title(my_xuid, &cand.title_id)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return TitleOutcome {
                title_id: title_id.clone(),
                name,
                result: TitleResult::Error(format!("fetch catalogue: {e}")),
            };
        }
    };

    // ============= UNLOCK-SAFETY GATE #2 (nuclear) ==============
    // Filter candidates: must be NOT yet achieved AND classify as
    // safe per the policy. Each skipped unsafe one gets an audit
    // entry. The catalogue we just fetched IS the source of truth
    // for metadata — no extra round-trip.
    use crate::plan::safety::{append_skip_audit, classify_achievement, SkipAuditEntry};
    let now_for_safety = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0);
    let pending: Vec<&crate::models::achievements::Achievement> = catalogue
        .achievements
        .iter()
        .filter(|a| !a.progress_state.eq_ignore_ascii_case("Achieved"))
        .filter(|a| {
            if !config.safety.is_active() {
                return true;
            }
            let class = classify_achievement(*a, config.safety, now_for_safety);
            if !class.is_safe() {
                let gs = a
                    .rewards
                    .iter()
                    .find(|r| r.ty.as_deref() == Some("Gamerscore"))
                    .and_then(|r| r.value.as_deref())
                    .and_then(|v| v.parse::<i32>().ok())
                    .unwrap_or(0);
                append_skip_audit(&SkipAuditEntry {
                    iso_timestamp: crate::plan::builder::iso_now_utc(),
                    reason: class.reason_str().to_string(),
                    title_id: cand.title_id.clone(),
                    title_name: name.clone(),
                    achievement_id: a.id.clone(),
                    achievement_name: a.name.clone(),
                    gamerscore: gs,
                    classification: class,
                    xuid: config.my_xuid.clone(),
                });
                return false;
            }
            true
        })
        .collect();
    let pending_ids: Vec<String> = pending.iter().map(|a| a.id.clone()).collect();

    if pending_ids.is_empty() {
        return TitleOutcome {
            title_id: title_id.clone(),
            name,
            result: TitleResult::AlreadyComplete,
        };
    }

    // Sum expected gamerscore of the (safe + pending) ones.
    let expected_gs: i32 = pending
        .iter()
        .filter_map(|a| {
            a.rewards
                .iter()
                .find(|r| r.ty.as_deref() == Some("Gamerscore"))
                .and_then(|r| r.value.as_deref())
                .and_then(|v| v.parse::<i32>().ok())
        })
        .sum();

    if client.is_event_based_title(&cand.title_id) {
        // Per-ID gamerscore lookup so partial success credits the
        // ACTUAL gs of the ids that landed (not `succeeded / pending
        // * expected_gs`, which mis-credits whenever mixed-value
        // achievements are involved).
        let gs_by_id: std::collections::HashMap<String, i32> = pending
            .iter()
            .map(|a| {
                let gs = a
                    .rewards
                    .iter()
                    .find(|r| r.ty.as_deref() == Some("Gamerscore"))
                    .and_then(|r| r.value.as_deref())
                    .and_then(|v| v.parse::<i32>().ok())
                    .unwrap_or(0);
                (a.id.clone(), gs)
            })
            .collect();
        // Validate title_id + xuid ONCE per candidate title. Per-id
        // validation inside the hot loop. Typed API prevents passing
        // achievement_id where title_id goes.
        let title_id_t = match crate::types::TitleId::try_from(cand.title_id.as_str()) {
            Ok(v) => v,
            Err(e) => {
                return TitleOutcome {
                    title_id: title_id.clone(),
                    name,
                    result: TitleResult::Error(format!("bad title_id: {e}")),
                };
            }
        };
        let xuid_t = match crate::types::Xuid::try_from(my_xuid) {
            Ok(v) => v,
            Err(e) => {
                return TitleOutcome {
                    title_id: title_id.clone(),
                    name,
                    result: TitleResult::Error(format!("bad xuid: {e}")),
                };
            }
        };
        let mut succeeded = 0usize;
        let mut succeeded_gs: i32 = 0;
        let mut last_error = None;
        for id in &pending_ids {
            let id_t = match crate::types::AchievementId::try_from(id.as_str()) {
                Ok(v) => v,
                Err(e) => {
                    last_error = Some(format!("bad achievement_id `{id}`: {e}"));
                    continue;
                }
            };
            match client.unlock_achievement(&title_id_t, &xuid_t, &id_t).await {
                Ok(_) => {
                    succeeded += 1;
                    if let Some(gs) = gs_by_id.get(id) {
                        succeeded_gs = succeeded_gs.saturating_add(*gs);
                    }
                }
                Err(e) => {
                    tracing::warn!(title_id = %cand.title_id, ach_id = %id, ?e, "nuclear event unlock failed");
                    last_error = Some(e.to_string());
                }
            }
        }
        if succeeded == 0 {
            return TitleOutcome {
                title_id: title_id.clone(),
                name,
                result: TitleResult::Error(format!(
                    "event-based unlock failed for all {} achievement(s){}",
                    pending_ids.len(),
                    last_error
                        .as_deref()
                        .map_or_else(String::new, |msg| format!(": {msg}"))
                )),
            };
        }
        // Full success: credit full expected_gs and report as Unlocked.
        // Partial: use the summed succeeded_gs AND surface a Partial
        // so the drain loop can warn the user that some IDs failed.
        if succeeded == pending_ids.len() {
            return TitleOutcome {
                title_id: title_id.clone(),
                name,
                result: TitleResult::Unlocked {
                    count: succeeded,
                    gamerscore: expected_gs,
                },
            };
        }
        let failed = pending_ids.len() - succeeded;
        let message = format!(
            "event-based unlock only {succeeded}/{} landed ({failed} failed){}",
            pending_ids.len(),
            last_error
                .as_deref()
                .map_or_else(String::new, |msg| format!("; last error: {msg}"))
        );
        return TitleOutcome {
            title_id: title_id.clone(),
            name,
            result: TitleResult::Partial {
                count: succeeded,
                gamerscore: succeeded_gs,
                failed,
                message,
            },
        };
    }

    // Resolve scid from the catalogue response. First achievement row
    // carries it (populated by the cache prewarm path). Fallback to
    // an explicit `resolve_service_config_id` call if the catalogue
    // didn't include one — that path populates the client's cache too.
    let scid_from_cat = catalogue
        .achievements
        .iter()
        .find(|a| !a.service_config_id.is_empty())
        .map(|a| a.service_config_id.clone());
    let scid = match scid_from_cat {
        Some(s) => s,
        None => match client
            .resolve_service_config_id(my_xuid, &cand.title_id)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                return TitleOutcome {
                    title_id: title_id.clone(),
                    name,
                    result: TitleResult::Error(format!("scid resolve: {e}")),
                };
            }
        },
    };

    let chunk_size = chunk_size_override.unwrap_or(crate::xbox::constants::UNLOCK_CHUNK_SIZE);
    match client
        .unlock_title_based_achievements_chunked(
            &scid,
            &cand.title_id,
            my_xuid,
            &pending_ids,
            chunk_size,
        )
        .await
    {
        Ok(()) => TitleOutcome {
            title_id: title_id.clone(),
            name,
            result: TitleResult::Unlocked {
                count: pending_ids.len(),
                gamerscore: expected_gs,
            },
        },
        Err(e) => {
            let msg = e.to_string();
            let event_based = msg.contains("\"code\":49")
                || msg.contains("code 49")
                || msg.contains("None of the submitted achievements");
            if event_based {
                TitleOutcome {
                    title_id: title_id.clone(),
                    name,
                    result: TitleResult::EventBasedSkipped,
                }
            } else {
                TitleOutcome {
                    title_id: title_id.clone(),
                    name,
                    result: TitleResult::Error(msg),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_nuclear_config_values() {
        let c = NuclearConfig::default();
        assert!(c.my_xuid.is_empty());
        assert_eq!(c.title_concurrency, 6);
        assert_eq!(c.chunk_size_override, None);
        assert_eq!(c.max_titles, 0);
        assert!(!c.include_already_complete);
    }

    #[test]
    fn starting_without_xuid_errors() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = XboxClient::new(crate::xbox::client::XboxClientOptions {
                xauth: "XBL3.0 x=1;dummydummydummydummydummydummy".into(),
                ..Default::default()
            })
            .unwrap();
            let cfg = NuclearConfig::default();
            let err = start_nuclear(client, cfg).err().unwrap();
            assert!(err.to_string().contains("my_xuid"));
        });
    }
}
