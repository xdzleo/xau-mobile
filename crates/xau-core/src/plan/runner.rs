//! Replay a [`Plan`](super::Plan) on the caller's own Xbox Live profile.
//!
//! The runner iterates the plan in the order stored (oldest unlock first)
//! and POSTs an achievement unlock for each entry. Optional spacing between
//! unlocks keeps the pattern from looking synthetic. Progress is surfaced
//! via a shared [`ReplayProgress`] struct readable from another thread.

// Fixed-range date arithmetic. All casts are sound for 1970..=9999.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::ref_option
)]

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::error::{Error, Result};
use crate::xbox::XboxClient;

use super::Plan;

/// Which strategy the runner uses to walk the plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayMode {
    /// Sequential, oldest-first, preserves original timing gaps between
    /// consecutive unlocks. Used when the goal is to "copy someone's
    /// journey" faithfully. Slow on big libraries.
    #[default]
    Mirror,
    /// Group entries by title, then unlock every achievement of each
    /// title in a single batched POST, running multiple titles in
    /// parallel. Ignores `time_unlocked` gaps entirely — the goal here
    /// is "max out everything as fast as the server allows". For a
    /// 4000-title / 126k-achievement profile this finishes in minutes
    /// instead of hours.
    BulkPerGame,
}

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    /// Delay between consecutive unlock POSTs. `Duration::ZERO` means
    /// "as fast as the server allows" (not recommended).
    pub delay_between: Duration,
    /// If `true`, waits between entries reproduce the **exact** original
    /// gaps between the target's `time_unlocked` timestamps (so a 10-minute
    /// pause in the source becomes a 10-minute pause in the replay).
    /// `min_delay` / `max_delay` act only as safety rails — both default to
    /// zero, which means "no clamp, mirror exactly". Set `max_delay` to a
    /// non-zero value to cap long waits; set `min_delay` to guarantee a
    /// floor between unlocks that were recorded in the same second.
    pub mirror_real_pacing: bool,
    /// Floor for mirror mode. `Duration::ZERO` = no floor.
    pub min_delay: Duration,
    /// Ceiling for mirror mode. `Duration::ZERO` = no ceiling (exact mirror).
    pub max_delay: Duration,
    /// Caller-supplied XUID. We don't re-probe it each entry.
    pub my_xuid: String,
    /// Stop the replay after this many successful unlocks (0 = no limit).
    pub max_entries: usize,
    /// Strategy for walking the plan. Defaults to `Mirror` for backwards
    /// compatibility; set to `BulkPerGame` for "max out everything fast".
    pub mode: ReplayMode,
    /// In `BulkPerGame` mode, how many titles to unlock in parallel.
    /// Default 6 — calibrated from **LIVE Xbox Live observation**
    /// (not mock). Raw 429 responses from the real unlock endpoint
    /// return:
    ///     `{"currentRequests":112,"maxRequests":100,"periodInSeconds":15}`
    ///
    /// So the update bucket is **100 req / 15s** = 6.67 RPS sustained.
    /// Concurrency=6 with ~1-2s per request keeps us inside that
    /// window. Concurrency=16 (previous default, from mock bench)
    /// stormed the budget in 1.25s and caused 93 consecutive errors
    /// in the bulk_replay_bench live run.
    ///
    /// If you need max throughput AND are willing to eat the rate-
    /// limit waits (now handled efficiently via a per-endpoint
    /// throttle gate, not per-request retry), raise this to 8-10.
    /// Past 10 the gate ends up open-closed-open-closed and you lose
    /// throughput to the constant flush. Ignored in `Mirror` mode.
    pub bulk_concurrency: usize,
    /// Override `UNLOCK_CHUNK_SIZE` (default 64) for the per-title
    /// batch POST. Set to `Some(1)` to force **strict per-game
    /// chronological order** — each achievement becomes its own POST
    /// so Xbox's server-side `timeUnlocked` timestamps increase
    /// strictly in the order the achievements appear in the source
    /// plan (= the target's real unlock order).
    ///
    /// Trade-off: N× more HTTP requests per title (N = achievements
    /// per title). Gears 5's 140 achievements go from 3 POSTs to 140.
    /// Net wall-clock rises modestly — the rate limiter paces us at
    /// 6.67 RPS anyway (100 req/15s), so the bound is the sustained
    /// budget, not the chunk size.
    ///
    /// `None` (default) uses `UNLOCK_CHUNK_SIZE = 64` — all batch
    /// achievements share one `timeUnlocked` server-side, so sub-chunk
    /// chronology within a batch is lost. `Some(1)` preserves it.
    pub chunk_size_override: Option<usize>,
    /// Unlock-safety policy applied at pre-unlock time (gate #2 —
    /// belt-and-suspenders after the plan-build filter). Default
    /// strict; opt into `danger_allow_unsafe` only via explicit CLI
    /// flag or GUI override.
    pub safety: crate::plan::safety::SafetyPolicy,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            delay_between: Duration::from_secs(30),
            mirror_real_pacing: false,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            my_xuid: String::new(),
            max_entries: 0,
            mode: ReplayMode::Mirror,
            bulk_concurrency: 6,
            chunk_size_override: None,
            safety: crate::plan::safety::SafetyPolicy::strict(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayState {
    #[default]
    Idle,
    Running,
    Paused,
    Done,
    Stopped,
    Error,
}

/// Per-achievement record of a failed unlock attempt. Populated by the
/// bulk runner so the GUI can show "X failed (click to retry)" and
/// dispatch a follow-up bulk run that targets only this list.
///
/// BUG HUNTER M-replay-failed-list: previously `ReplayProgress` only
/// surfaced an `errors: usize` counter and a single `last_error`
/// string. The user couldn't see WHICH achievements failed without
/// tailing `replay-errors.jsonl` (and even that file only had
/// title-level failures). With per-achievement records the GUI can
/// keep a live failure queue and offer one-click retry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FailedEntry {
    pub title_id: String,
    pub game_name: String,
    pub achievement_id: String,
    pub achievement_name: String,
    pub service_config_id: String,
    pub gamerscore: i32,
    pub error: String,
    /// ISO-8601 simple timestamp of when the failure was recorded.
    pub ts: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayProgress {
    pub state: ReplayState,
    pub total: usize,
    pub done: usize,
    pub current_title: Option<String>,
    pub current_achievement: Option<String>,
    pub errors: usize,
    pub last_error: Option<String>,
    /// Wave-15 audit race-M4: monotonic run identifier. Bumped on
    /// every `start_replay` so a "retry failed" IPC initiated against
    /// an OLDER snapshot can be rejected — the failed_entries list
    /// belongs to a different run by that point. Lives in the
    /// snapshot — clients that read `failed_entries` MUST echo this
    /// back when they request retry. Zero is the default for
    /// never-started; first run is 1.
    #[serde(default)]
    pub run_id: u64,
    /// Total gamerscore accumulated from successfully-unlocked entries
    /// since the replay started. Updated per-unlock so the UI can show
    /// "earned 2340G" live.
    #[serde(default)]
    pub gamerscore_earned: i32,
    /// Number of unique titles already skipped because the server
    /// rejected them with code 49 (event-based, not in the bundle).
    /// Surfaced so the UI can say "skipped 12 unsupported games".
    #[serde(default)]
    pub skipped_titles: usize,
    /// Per-achievement records of unlocks that didn't land. Capped at
    /// 5000 entries to keep snapshot serialization cheap on
    /// SiegfriedX-sized plans (8k attempts × ~200 bytes = 1.6 MiB worst
    /// case). The complete record always lives in
    /// `unlock-history.jsonl` regardless of this cap.
    #[serde(default)]
    pub failed_entries: Vec<FailedEntry>,
    /// Path to the per-achievement history log so the GUI can offer
    /// a "Open log" button. None until the runner has actually
    /// resolved a writable path.
    #[serde(default)]
    pub history_log_path: Option<String>,
}

#[derive(Clone)]
pub struct ReplayHandle {
    shared: Arc<RwLock<ReplayProgress>>,
    stop_tx: Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

/// Wave-15 audit race-M4: process-wide monotonic counter for replay
/// runs. Strictly increasing across every `start_replay` invocation;
/// used by the IPC layer to detect "retry failed against a newer run"
/// stale-data races. First run is 1 (zero is reserved for "no replay
/// has ever run" / default).
static REPLAY_RUN_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[must_use]
fn next_replay_run_id() -> u64 {
    REPLAY_RUN_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

impl ReplayHandle {
    pub fn snapshot(&self) -> ReplayProgress {
        self.shared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Cheap accessor for the monotonic run identifier. The IPC
    /// `replay_retry_failed` path uses this to reject retry calls
    /// initiated against a prior run's `failed_entries` list. See
    /// race-M4.
    #[must_use]
    pub fn run_id(&self) -> u64 {
        self.shared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .run_id
    }

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
            ReplayState::Running | ReplayState::Paused
        )
    }
}

/// Kick off a replay as a detached tokio task. Returns a handle the caller
/// uses to observe progress and later stop the run.
pub fn start_replay(
    client: XboxClient,
    mut plan: Plan,
    config: ReplayConfig,
) -> Result<ReplayHandle> {
    if plan.is_empty() {
        return Err(Error::invalid_argument("plan is empty"));
    }
    if config.my_xuid.trim().is_empty() {
        return Err(Error::invalid_argument("my_xuid is required"));
    }
    // BBB-Discontinued: same upstream filter the bit-by-bit runner
    // uses. Replay also POSTs unlocks, so blocked titles must be
    // dropped before the replay loop runs. See
    // `plan::discontinued` for the blocklist source.
    let blocklist = crate::plan::discontinued::load_discontinued_set();
    let removed = crate::plan::discontinued::filter_plan_remove_discontinued(&mut plan, &blocklist);
    if !removed.is_empty() {
        let total: usize = removed.iter().map(|r| r.entry_count).sum();
        tracing::warn!(
            titles = removed.len(),
            entries = total,
            "start_replay: filtered discontinued titles before scheduling"
        );
        for r in &removed {
            crate::plan::safety::append_skip_audit(&crate::plan::safety::SkipAuditEntry {
                iso_timestamp: iso_now_simple_public(),
                reason: format!(
                    "discontinued blocklist match — filtered {} entries upstream of replay loop",
                    r.entry_count
                ),
                title_id: r.title_id.clone(),
                title_name: r.game_name.clone(),
                achievement_id: String::new(),
                achievement_name: String::new(),
                gamerscore: 0,
                classification: crate::plan::safety::SafetyClass::Discontinued,
                xuid: config.my_xuid.clone(),
            });
        }
    }
    if plan.is_empty() {
        return Err(Error::invalid_argument(
            "every title in this plan is on the discontinued blocklist — \
             nothing left to replay. Edit `Documents/XAU/discontinued.txt` \
             if you want to override.",
        ));
    }
    // BUG HUNTER runner-lock (#12): acquire the per-xuid runner lock
    // BEFORE the spawn so a collision surfaces synchronously on the
    // caller (UI / CLI). Holding it in the spawned task keeps the
    // exclusion alive until the runner returns; Drop on completion
    // (success, error, or stop) releases it for the next run.
    let runner_lock = crate::plan::runner_lock::RunnerLock::try_acquire(&config.my_xuid)?;

    let progress = ReplayProgress {
        state: ReplayState::Running,
        // Wave-15 audit race-M4: bump the global run_id so any stale
        // "retry failed" IPC initiated against a prior snapshot can
        // be rejected by mismatch.
        run_id: next_replay_run_id(),
        total: effective_replay_total(&plan, config.max_entries),
        done: 0,
        current_title: None,
        current_achievement: None,
        errors: 0,
        last_error: None,
        gamerscore_earned: 0,
        skipped_titles: 0,
        failed_entries: Vec::new(),
        history_log_path: unlock_history_log_path()
            .and_then(|p| p.into_os_string().into_string().ok()),
    };
    let shared = Arc::new(RwLock::new(progress));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let stop_holder = Arc::new(std::sync::Mutex::new(Some(stop_tx)));

    let handle = ReplayHandle {
        shared: shared.clone(),
        stop_tx: stop_holder,
    };

    // Wave-6 audit state-F1: previously this `tokio::spawn` was
    // unsupervised — a panic inside `run()` (e.g., a poisoned mutex
    // inside `unlock_achievement`, an arithmetic overflow on a
    // borked timestamp, a serde_json::to_string failure) would drop
    // the task while `state == Running` was still committed. The
    // `ReplayHandle::is_running()` predicate then returned `true`
    // forever, the IPC's "previous replay still stopping" guard
    // rejected every subsequent run, and the user had to restart
    // the GUI. Wrap the work in a supervisor that observes the
    // `JoinError` and force-flips state to `Error` so `is_running()`
    // unsticks. Mirror's the bit_by_bit supervisor pattern at
    // bit_by_bit.rs:1081-1129.
    let shared_for_recovery = shared.clone();
    tokio::spawn(async move {
        let _lock = runner_lock; // hold for the duration of the run
        let inner = tokio::spawn(async move {
            run(client, plan, config, shared, stop_rx).await;
        });
        match inner.await {
            Ok(()) => {}
            Err(je) => {
                let reason = if je.is_panic() {
                    format!("replay run panicked — task supervised: {je}")
                } else if je.is_cancelled() {
                    "replay run cancelled unexpectedly (runtime shutdown?)".to_string()
                } else {
                    format!("replay run terminated abnormally: {je}")
                };
                tracing::error!(
                    ?je,
                    "replay run() ended abnormally — flipping state to Error"
                );
                let mut w = shared_for_recovery
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Preserve a real terminal state if `run()` already
                // set one (Done/Stopped/Error) — the work landed and
                // the panic was on the way out. Otherwise overwrite
                // Running/Paused with Error so `is_running()` flips.
                match w.state {
                    ReplayState::Done | ReplayState::Stopped | ReplayState::Error => {}
                    _ => {
                        w.state = ReplayState::Error;
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

fn effective_replay_total(plan: &Plan, max_entries: usize) -> usize {
    let mut seen: std::collections::HashSet<(&str, &str)> =
        std::collections::HashSet::with_capacity(plan.entries.len());
    let deduped = plan
        .entries
        .iter()
        .filter(|e| seen.insert((e.title_id.as_str(), e.achievement_id.as_str())))
        .count();
    if max_entries == 0 {
        deduped
    } else {
        max_entries.min(deduped)
    }
}

async fn run(
    client: XboxClient,
    plan: Plan,
    config: ReplayConfig,
    shared: Arc<RwLock<ReplayProgress>>,
    stop_rx: oneshot::Receiver<()>,
) {
    match config.mode {
        ReplayMode::Mirror => run_mirror(client, plan, config, shared, stop_rx).await,
        ReplayMode::BulkPerGame => run_bulk(client, plan, config, shared, stop_rx).await,
    }
}

async fn run_mirror(
    client: XboxClient,
    plan: Plan,
    config: ReplayConfig,
    shared: Arc<RwLock<ReplayProgress>>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    // BUG HUNTER RN-N1: defensive dedupe at runner entry — plans
    // produced by `build_plan` or `best_of_aggregate` are already
    // deduped, but loaded-from-disk plans (corrupted, older
    // aggregate outputs pre-N16 fix, hand-edited) might carry
    // internal duplicates. Sending the same POST twice wastes HTTP
    // budget + rate-limit tokens without adding any credit (Xbox
    // returns 200 OK on the dup but doesn't re-credit). Dedupe here
    // so every runner variant benefits from the same guarantee.
    let mut seen: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::with_capacity(plan.entries.len());
    let mut entries = plan.entries;
    entries.retain(|e| seen.insert((e.title_id.clone(), e.achievement_id.clone())));
    let cap = if config.max_entries == 0 {
        entries.len()
    } else {
        config.max_entries.min(entries.len())
    };
    shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .total = cap;

    let mut previous_ts: Option<String> = None;
    // Titles that rejected an unlock with "code 49 · None of the submitted
    // achievements may be updated in this fashion" — the server-side tell
    // that they're event-based. We skip every subsequent entry for that
    // title instead of burning through them one by one.
    let mut blacklisted_titles: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for (idx, entry) in entries.into_iter().take(cap).enumerate() {
        if stop_rx.try_recv().is_ok() {
            shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .state = ReplayState::Stopped;
            return;
        }

        // If we've already learned this title can't be unlocked via the
        // modern endpoint, skip silently — no pacing wait, no HTTP call.
        if blacklisted_titles.contains(&entry.title_id) {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.skipped_titles = blacklisted_titles.len();
            continue;
        }

        {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.current_title = Some(entry.game_name.clone());
            w.current_achievement = Some(entry.achievement_name.clone());
        }

        // Figure out the pacing between this entry and the previous one.
        let wait = pacing_between(&previous_ts, &entry.time_unlocked, idx == 0, &config);

        if !wait.is_zero() {
            tokio::select! {
                biased;
                _ = &mut stop_rx => {
                    shared.write().unwrap_or_else(std::sync::PoisonError::into_inner).state = ReplayState::Stopped;
                    return;
                }
                () = tokio::time::sleep(wait) => {}
            }
        }

        // Convert to typed newtypes at the boundary. Per-entry
        // validation is cheap (~1µs); invalid strings surface at
        // the HANDLER level as a concrete validation error instead
        // of a mystery Xbox 400. The typed unlock_achievement sig
        // also prevents arg-order swaps at compile time — title_id
        // can't be passed where achievement_id goes.
        let result = match crate::plan::safety::filter_unlock_safe(
            &client,
            &config.my_xuid,
            &entry.title_id,
            &entry.game_name,
            std::slice::from_ref(&entry.achievement_id),
            config.safety,
        )
        .await
        {
            Ok((safe_ids, skipped)) if safe_ids.is_empty() => Err(Error::other(format!(
                "skipped by replay safety gate ({skipped} unsafe achievement(s))"
            ))),
            Ok((safe_ids, _)) => match (
                crate::types::TitleId::try_from(entry.title_id.as_str()),
                crate::types::Xuid::try_from(config.my_xuid.as_str()),
                crate::types::AchievementId::try_from(safe_ids[0].as_str()),
            ) {
                (Ok(t), Ok(x), Ok(a)) => {
                    // Wave-8 audit cancel-HIGH-2: previously this was
                    // a bare `await` — a Stop click during a slow
                    // unlock POST (which the retry deadline now caps
                    // at 180s, but in-flight reqwest is still
                    // un-cancellable) waited for the full retry
                    // chain before the next iteration's
                    // `stop_rx.try_recv()` could observe the cancel.
                    // Race the POST against `stop_rx` so cancel takes
                    // effect at the network-IO boundary instead.
                    // Cancel arm flips state to Stopped and returns;
                    // outer bookkeeping path is bypassed entirely
                    // (the unlock didn't land — no credit owed).
                    tokio::select! {
                        biased;
                        _ = &mut stop_rx => {
                            shared.write().unwrap_or_else(std::sync::PoisonError::into_inner).state = ReplayState::Stopped;
                            return;
                        }
                        r = client.unlock_achievement(&t, &x, &a) => r.map(|_| ()),
                    }
                }
                (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                    Err(Error::invalid_argument(format!("typed validation: {e}")))
                }
            },
            Err(e) => Err(e),
        };

        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match result {
            Ok(()) => {
                w.done += 1;
                w.last_error = None;
                if let Some(gs) = entry.gamerscore {
                    w.gamerscore_earned = w.gamerscore_earned.saturating_add(gs);
                }
            }
            Err(e) => {
                let msg = e.to_string();
                // Detect Xbox's "event-based, can't update here" signal
                // (HTTP 400 with {"code":49,...}). Blacklist the title
                // entirely so we don't retry 699 more entries from it.
                let looks_event_based = msg.contains("\"code\":49")
                    || msg.contains("code 49")
                    || msg.contains("None of the submitted achievements");
                if looks_event_based {
                    blacklisted_titles.insert(entry.title_id.clone());
                    w.skipped_titles = blacklisted_titles.len();
                    w.last_error = Some(format!(
                        "{} — event-based title (not in unlock bundle), skipping all its achievements",
                        entry.game_name
                    ));
                    tracing::info!(
                        game = %entry.game_name,
                        title_id = %entry.title_id,
                        "title is event-based — blacklisting for this replay"
                    );
                } else {
                    w.errors += 1;
                    w.last_error = Some(format!("{}: {e}", entry.achievement_name));
                    // Wave-9 audit state-F5: previously the mirror runner
                    // populated `errors` + `last_error` but NEVER added
                    // anything to `failed_entries`. The GUI's "X failed
                    // (click to retry)" feature therefore had nothing to
                    // dispatch from for mirror-mode runs — silently a
                    // no-op. Mirror the BulkPerGame path's append at
                    // line ~852 so retry-failed works uniformly across
                    // modes. We bound `failed_entries.len()` at the
                    // existing `FAILED_CAP` (see line ~876).
                    const FAILED_CAP: usize = 5_000;
                    if w.failed_entries.len() < FAILED_CAP {
                        w.failed_entries.push(FailedEntry {
                            title_id: entry.title_id.clone(),
                            game_name: entry.game_name.clone(),
                            achievement_id: entry.achievement_id.clone(),
                            achievement_name: entry.achievement_name.clone(),
                            service_config_id: entry.service_config_id.clone(),
                            gamerscore: entry.gamerscore.unwrap_or(0),
                            error: e.to_string(),
                            ts: iso_now_simple(),
                        });
                    }
                    tracing::warn!(game = %entry.game_name, ach = %entry.achievement_name, %e, "replay entry failed");
                }
            }
        }
        drop(w);
        previous_ts = Some(entry.time_unlocked);
    }

    shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .state = ReplayState::Done;
}

/// "Max out everything" runner. Groups the plan entries by title,
/// unlocks every achievement of each title in one batched POST
/// (Xbox accepts up to 50 IDs per request — `unlock_title_based_achievements`
/// chunks internally if the list is larger), and runs multiple titles
/// in parallel bounded by `config.bulk_concurrency`.
///
/// For a SiegfriedX-sized plan (8463 entries across 687 titles) this
/// typically finishes in **2-5 minutes** vs the mirror runner's
/// 12+ hours. Timing gaps are ignored by design — this mode is
/// explicitly for "give me everything now".
#[allow(
    clippy::items_after_statements,
    clippy::type_complexity,
    clippy::too_many_lines
)]
async fn run_bulk(
    client: XboxClient,
    plan: Plan,
    config: ReplayConfig,
    shared: Arc<RwLock<ReplayProgress>>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    // BUG HUNTER RN-N1: defensive dedupe at runner entry (same
    // rationale as `run_mirror`). Corrupted/old plans with internal
    // duplicates would otherwise trigger duplicate POSTs inside a
    // title's unlock batch — Xbox returns 200 OK but no credit,
    // just wasted budget.
    let mut seen: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::with_capacity(plan.entries.len());
    let mut deduped = plan.entries;
    deduped.retain(|e| seen.insert((e.title_id.clone(), e.achievement_id.clone())));
    // Respect max_entries by truncating the plan up front.
    let entries: Vec<_> = if config.max_entries == 0 {
        deduped
    } else {
        deduped.into_iter().take(config.max_entries).collect()
    };

    // Group by title_id. For each title we remember:
    //   - display name
    //   - serviceConfigId (from any entry — they all share one per title)
    //   - list of (achievement_id, achievement_name, gamerscore)
    // Keep title insertion order so progress feels natural.
    //
    // BUG HUNTER M-replay-history: previously the per-achievement
    // tuple was (id, gs) — the `achievement_name` from the source
    // Plan entry was discarded during this flatten. The new
    // unlock-history.jsonl writer + the GUI's failed-list need the
    // human-readable name, so we now keep all three fields. Costs
    // ~50 bytes per entry × 8k entries on a SiegfriedX-sized plan
    // ≈ 400 KB peak — negligible.
    use std::collections::HashMap;
    let mut by_title: HashMap<String, TitleSlot> = HashMap::new();
    let mut title_order: Vec<String> = Vec::new();
    for e in entries {
        let slot = by_title.entry(e.title_id.clone()).or_insert_with(|| {
            title_order.push(e.title_id.clone());
            (e.game_name.clone(), e.service_config_id.clone(), Vec::new())
        });
        slot.2.push((
            e.achievement_id,
            e.achievement_name,
            e.gamerscore.unwrap_or(0),
        ));
    }

    let total_titles = title_order.len();
    let total_entries: usize = by_title.values().map(|(_, _, ids)| ids.len()).sum();
    tracing::info!(
        titles = total_titles,
        total_entries,
        concurrency = config.bulk_concurrency,
        "bulk replay starting"
    );
    shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .total = total_entries;

    // JoinSet with bounded concurrency. Each task unlocks one title's
    // full achievement list and returns the per-achievement outcome
    // so the drain loop can populate ReplayProgress.failed_entries
    // and write the per-achievement unlock-history.jsonl log.
    // Types hoisted out of the function body to keep clippy happy
    // (items_after_statements + type_complexity).
    type TitleSlot = (String, String, Vec<(String, String, i32)>);
    // (game_name, scid, Vec<(ach_id, ach_name, gamerscore)>)

    #[allow(dead_code)] // title_id kept for tracing
    struct TitleResult {
        title_id: String,
        game_name: String,
        scid: String,
        attempted: usize,
        succeeded: usize,
        gamerscore: i32,
        event_based_skip: bool,
        error: Option<String>,
        /// FULL list of (id, name, gs) we tried for this title — used by
        /// the drain loop to derive the failed subset and write the
        /// per-achievement history log.
        ach_list: Vec<(String, String, i32)>,
        /// IDs the server accepted (subset of ach_list).
        succeeded_ids: Vec<String>,
    }
    let mut joins: tokio::task::JoinSet<TitleResult> = tokio::task::JoinSet::new();
    let mut iter = title_order.into_iter();

    let chunk_size_override = config.chunk_size_override;
    let safety_policy = config.safety;
    let cancelled = Arc::new(AtomicBool::new(false));
    let spawn_title = |joins: &mut tokio::task::JoinSet<TitleResult>,
                       title_id: String,
                       by_title: &mut HashMap<String, TitleSlot>| {
        let Some((game_name, scid, ach_list)) = by_title.remove(&title_id) else {
            return;
        };
        let client = client.clone();
        let xuid = config.my_xuid.clone();
        let cancelled = cancelled.clone();
        let scid_for_task = scid.clone();
        joins.spawn(async move {
            let ids: Vec<String> = ach_list.iter().map(|(id, _, _)| id.clone()).collect();
            let attempted = ids.len();
            // `unlock_title_batch` (below) picks the events path
            // when the title is in the bundle, otherwise does a
            // single bulk POST to /achievements/update. It returns
            // the EXACT list of achievement IDs that landed — so
            // we can sum gamerscore against the real successes,
            // not the first N items in order.
            match unlock_title_batch(
                &client,
                &title_id,
                &scid,
                &xuid,
                &ids,
                chunk_size_override,
                safety_policy,
                cancelled,
            )
            .await
            {
                Ok(outcome) => {
                    let gs = credit_gamerscore_named(&ach_list, &outcome.succeeded_ids);
                    let succeeded = outcome.succeeded_ids.len();
                    // BUG HUNTER M-runner-safety-skipped: subtract
                    // safety-gated entries from the "attempted"
                    // budget when classifying partials so the
                    // errors counter only reflects genuine server
                    // rejections. Safety skips are already audited
                    // separately in unlock-safety-skipped.jsonl.
                    let real_attempted = attempted.saturating_sub(outcome.safety_skipped);
                    let partial_note = if succeeded < real_attempted {
                        Some(format!(
                            "{game_name} — only {succeeded}/{real_attempted} achievements landed \
                             ({} server rejections; {} skipped by safety gate)",
                            real_attempted - succeeded,
                            outcome.safety_skipped,
                        ))
                    } else {
                        None
                    };
                    TitleResult {
                        title_id,
                        game_name,
                        scid: scid_for_task,
                        attempted: real_attempted,
                        succeeded,
                        gamerscore: gs,
                        event_based_skip: false,
                        error: partial_note,
                        ach_list,
                        succeeded_ids: outcome.succeeded_ids,
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    let event_based = msg.contains("\"code\":49")
                        || msg.contains("code 49")
                        || msg.contains("None of the submitted achievements");
                    TitleResult {
                        title_id,
                        game_name,
                        scid: scid_for_task,
                        attempted,
                        succeeded: 0,
                        gamerscore: 0,
                        event_based_skip: event_based,
                        error: Some(msg),
                        ach_list,
                        succeeded_ids: Vec::new(),
                    }
                }
            }
        });
    };

    // Prime the pool.
    let parallel = config.bulk_concurrency.max(1);
    for _ in 0..parallel {
        if let Some(tid) = iter.next() {
            spawn_title(&mut joins, tid, &mut by_title);
        } else {
            break;
        }
    }

    loop {
        let joined = tokio::select! {
            biased;
            _ = &mut stop_rx => {
                cancelled.store(true, Ordering::SeqCst);
                joins.abort_all();
                shared.write().unwrap_or_else(std::sync::PoisonError::into_inner).state = ReplayState::Stopped;
                return;
            }
            joined = joins.join_next() => joined,
        };
        let Some(joined) = joined else {
            break;
        };
        match joined {
            Ok(res) => {
                // Compute the success/failure split BEFORE taking the
                // shared write lock — we want the lock held for as
                // little time as possible so progress.tick listeners
                // don't block on the per-achievement reconciliation.
                let succeeded_set: std::collections::HashSet<&String> =
                    res.succeeded_ids.iter().collect();
                let ts = iso_now_simple();

                // BUG HUNTER M-replay-history: emit one history line
                // PER achievement (success / failed / skipped), and
                // populate the in-memory failed_entries Vec for the
                // GUI. Skipped event-based titles get every entry
                // logged as `status:"skipped"` so the user sees the
                // full picture in unlock-history.jsonl.
                let mut new_failures: Vec<FailedEntry> = Vec::new();
                for (ach_id, ach_name, ach_gs) in &res.ach_list {
                    if res.event_based_skip {
                        // Whole title skipped — every ach is "skipped".
                        let line = serde_json::json!({
                            "ts": ts,
                            "title_id": res.title_id,
                            "game": res.game_name,
                            "scid": res.scid,
                            "ach_id": ach_id,
                            "ach_name": ach_name,
                            "gamerscore": ach_gs,
                            "status": "skipped",
                            "reason": "event_based_not_in_bundle",
                        })
                        .to_string();
                        let _ = append_history_line(&line);
                        continue;
                    }
                    if succeeded_set.contains(ach_id) {
                        let line = serde_json::json!({
                            "ts": ts,
                            "title_id": res.title_id,
                            "game": res.game_name,
                            "scid": res.scid,
                            "ach_id": ach_id,
                            "ach_name": ach_name,
                            "gamerscore": ach_gs,
                            "status": "success",
                        })
                        .to_string();
                        let _ = append_history_line(&line);
                    } else {
                        // Genuine failure — server rejected this ach
                        // (or rejected the whole chunk it was in). The
                        // error message at title-level explains why.
                        let err_msg = res
                            .error
                            .clone()
                            .unwrap_or_else(|| "unknown server rejection".to_string());
                        let line = serde_json::json!({
                            "ts": ts,
                            "title_id": res.title_id,
                            "game": res.game_name,
                            "scid": res.scid,
                            "ach_id": ach_id,
                            "ach_name": ach_name,
                            "gamerscore": ach_gs,
                            "status": "failed",
                            "error": err_msg,
                        })
                        .to_string();
                        let _ = append_history_line(&line);
                        new_failures.push(FailedEntry {
                            title_id: res.title_id.clone(),
                            game_name: res.game_name.clone(),
                            achievement_id: ach_id.clone(),
                            achievement_name: ach_name.clone(),
                            service_config_id: res.scid.clone(),
                            gamerscore: *ach_gs,
                            error: err_msg,
                            ts: ts.clone(),
                        });
                    }
                }

                let mut w = shared
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                w.current_title = Some(res.game_name.clone());
                w.current_achievement =
                    Some(format!("{} unlocked ({}G)", res.succeeded, res.gamerscore));
                w.done = w.done.saturating_add(res.succeeded);
                w.gamerscore_earned = w.gamerscore_earned.saturating_add(res.gamerscore);

                // Append to the live failed list, capped so a runaway
                // failure storm doesn't bloat snapshot serialization.
                // Older entries are dropped when we hit the cap — the
                // full record always lives in unlock-history.jsonl.
                const FAILED_CAP: usize = 5000;
                if !new_failures.is_empty() {
                    let total_after = w.failed_entries.len() + new_failures.len();
                    if total_after > FAILED_CAP {
                        let drop = total_after - FAILED_CAP;
                        let cur_len = w.failed_entries.len();
                        let drop_from_existing = drop.min(cur_len);
                        w.failed_entries.drain(0..drop_from_existing);
                    }
                    w.failed_entries.extend(new_failures);
                }

                if res.event_based_skip {
                    w.skipped_titles = w.skipped_titles.saturating_add(1);
                    w.last_error = Some(format!(
                        "{} — event-based (not in bundle), all {} achievements skipped",
                        res.game_name, res.attempted
                    ));
                } else if let Some(err) = res.error {
                    // Increment `errors` by the number that actually
                    // failed (`attempted - succeeded`), not by the
                    // full attempted count. Old code overcounted
                    // partials — a 3-of-5 title added 5 to the
                    // error tally AND 3 to `done`, so the HUD
                    // reported 8 items from a 5-item title.
                    let failed = res.attempted.saturating_sub(res.succeeded);
                    w.errors = w.errors.saturating_add(failed);
                    w.last_error = Some(format!("{}: {err}", res.game_name));
                    // Persistent title-level error log (kept for
                    // backwards compat with anything that tails it).
                    let line = serde_json::json!({
                        "ts": ts,
                        "title_id": res.title_id,
                        "game_name": res.game_name,
                        "attempted": res.attempted,
                        "error": err,
                    })
                    .to_string();
                    if let Err(e) = append_error_line(&line) {
                        tracing::warn!(?e, "could not append to replay-errors.jsonl");
                    }
                    tracing::warn!(
                        title_id = %res.title_id,
                        game = %res.game_name,
                        attempted = res.attempted,
                        err = %err,
                        "title batch failed"
                    );
                } else {
                    w.last_error = None;
                }
            }
            Err(e) => {
                let mut w = shared
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                w.errors = w.errors.saturating_add(1);
                w.last_error = Some(format!("bulk title task failed: {e}"));
                tracing::warn!(?e, "bulk task join failed");
            }
        }
        if let Some(next_tid) = iter.next() {
            spawn_title(&mut joins, next_tid, &mut by_title);
        }
    }

    shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .state = ReplayState::Done;
}

/// Unlock a batch of achievements for one title. Routes through the
/// events-bundle path when the title is event-based, otherwise uses the
/// direct `/achievements/update` bulk POST. Returns the count that
/// succeeded (so the bulk runner can credit partial gamerscore).
/// Returned by `unlock_title_batch` so the caller can attribute
/// gamerscore to the EXACT achievements that landed (not just a count),
/// which matters for the events path where individual POSTs can fail
/// while others succeed.
pub(crate) struct BatchOutcome {
    /// Achievement IDs that the server accepted. Length == successful
    /// unlock count. Order follows the input `ids` slice.
    pub succeeded_ids: Vec<String>,
    /// Count of IDs filtered by the safety gate BEFORE we POSTed.
    /// These are not failures (no server call was made); the drain
    /// loop subtracts them from the error tally so the HUD's
    /// "errors" counter only reflects genuine server rejections.
    /// BUG HUNTER M-runner-safety-skipped: previously the runner
    /// counted safety-gated entries as failed, inflating the
    /// errors number by the number of revoked / time-windowed
    /// achievements (often dozens per scan).
    /// Wave-13 audit dead-code-A1: previously had a stale
    /// `#[allow(dead_code)]` claiming "only the drain loop reads
    /// it" — but the drain DOES read it (line ~713 via
    /// `outcome.safety_skipped`). Allow removed.
    pub safety_skipped: usize,
}

/// Sum gamerscore of achievements that actually landed. Pure function
/// extracted from `run_bulk` so tests can lock the invariant:
/// gamerscore accounting is a **set membership lookup**, not an
/// `ids.iter().take(N)` prefix-sum. The old impl was wrong when the
/// events-path failed individual IDs out of order.
///
/// Production code now uses the 3-tuple `credit_gamerscore_named`
/// variant; this 2-tuple form is kept for the historical test
/// suite that locks the same invariant on a simpler shape. Gated
/// to `cfg(test)` so it doesn't generate a dead-code warning in
/// release builds.
#[cfg(test)]
pub(crate) fn credit_gamerscore(ach_list: &[(String, i32)], succeeded_ids: &[String]) -> i32 {
    let ok: std::collections::HashSet<&String> = succeeded_ids.iter().collect();
    ach_list
        .iter()
        .filter(|(id, _)| ok.contains(id))
        .map(|(_, gs)| *gs)
        .sum()
}

/// Same as `credit_gamerscore` but for the 3-tuple `(id, name, gs)`
/// shape the bulk runner uses internally (the `name` is dragged
/// through so the per-achievement history log can show human-readable
/// titles). Avoids reallocating an intermediate 2-tuple Vec on the
/// hot path.
pub(crate) fn credit_gamerscore_named(
    ach_list: &[(String, String, i32)],
    succeeded_ids: &[String],
) -> i32 {
    let ok: std::collections::HashSet<&String> = succeeded_ids.iter().collect();
    ach_list
        .iter()
        .filter(|(id, _, _)| ok.contains(id))
        .map(|(_, _, gs)| *gs)
        .sum()
}

async fn unlock_title_batch(
    client: &XboxClient,
    title_id: &str,
    scid_hint: &str,
    xuid: &str,
    ids: &[String],
    chunk_size: Option<usize>,
    safety: crate::plan::safety::SafetyPolicy,
    cancelled: Arc<AtomicBool>,
) -> Result<BatchOutcome> {
    // Defensive: empty ids shouldn't happen in practice (caller groups
    // entries by title, so any group has >=1 id), but a panic here
    // would leak into the replay runner's JoinSet and corrupt the
    // progress count.
    if ids.is_empty() {
        return Ok(BatchOutcome {
            succeeded_ids: Vec::new(),
            safety_skipped: 0,
        });
    }
    let original_count = ids.len();

    // ============= UNLOCK-SAFETY GATE #2 ==============
    // Belt-and-suspenders check before any POST. The plan builder
    // already filtered at build time (gate #1), but plans can:
    //   - pre-date this filter (loaded from disk)
    //   - be hand-edited
    //   - contain achievements whose `is_revoked` flipped between
    //     build time and replay time (rare, not impossible)
    // So we re-fetch live metadata and re-classify. Every skip
    // writes an audit entry. Inactive policy short-circuits.
    let (filtered_ids, safety_skipped_count) = crate::plan::safety::filter_unlock_safe(
        client, xuid, title_id, /* title_name */ "", ids, safety,
    )
    .await?;
    let safety_skipped = original_count
        .saturating_sub(filtered_ids.len())
        .max(safety_skipped_count);
    let ids: &[String] = &filtered_ids;
    if ids.is_empty() {
        return Ok(BatchOutcome {
            succeeded_ids: Vec::new(),
            safety_skipped,
        });
    }

    // Pick the right path. `unlock_achievement` does the event-vs-direct
    // routing for a single id — we call it per id iff the title is
    // event-based, otherwise do a single bulk POST.
    if client.is_event_based_title(title_id) {
        // Validate title_id and xuid ONCE per batch (not per id) so
        // the per-achievement loop is just a typed `&AchievementId`
        // per iteration. Invalid inputs surface as a typed error
        // instead of unbounded Xbox 400s.
        let title_id_t = crate::types::TitleId::try_from(title_id)
            .map_err(|e| Error::invalid_argument(format!("title_id `{title_id}`: {e}")))?;
        let xuid_t = crate::types::Xuid::try_from(xuid)
            .map_err(|e| Error::invalid_argument(format!("xuid `{xuid}`: {e}")))?;
        let mut succeeded_ids = Vec::with_capacity(ids.len());
        let mut failed_ids = Vec::new();
        let mut last_error = None;
        // BUG HUNTER cancel-credit (#14): previously a cancellation
        // mid-loop did `return Err("replay stopped")`, which dropped
        // every `succeeded_ids` that HAD already landed on Xbox.
        // Achievements that were really unlocked were reported to the
        // user as "stopped, nothing done", and the next replay would
        // re-submit them (Xbox 304s, but our bookkeeping double-counts
        // the failure tally). Break out instead, then return Ok with
        // the partial succeeded list — the caller's outer cancellation
        // check halts the next iteration anyway.
        let mut cancelled_mid_loop = false;
        for id in ids {
            if cancelled.load(Ordering::SeqCst) {
                cancelled_mid_loop = true;
                break;
            }
            let id_t = match crate::types::AchievementId::try_from(id.as_str()) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(title_id, ach_id = %id, ?e, "invalid achievement id — skipping");
                    failed_ids.push(id.clone());
                    last_error = Some(e.to_string());
                    continue;
                }
            };
            match client.unlock_achievement(&title_id_t, &xuid_t, &id_t).await {
                Ok(_) => succeeded_ids.push(id.clone()),
                Err(e) => {
                    tracing::warn!(title_id, ach_id = %id, ?e, "event-based replay unlock failed");
                    failed_ids.push(id.clone());
                    last_error = Some(e.to_string());
                }
            }
        }
        // If the user cancelled BEFORE any id landed, surface the
        // stop reason — there is no partial credit worth preserving,
        // and an empty BatchOutcome would be miscounted as a failure
        // tally by the bulk runner.
        if cancelled_mid_loop && succeeded_ids.is_empty() {
            return Err(Error::other("replay stopped"));
        }
        if succeeded_ids.is_empty() {
            return Err(Error::other(format!(
                "event-based unlock failed for all {} achievement(s) on title {title_id}{}",
                ids.len(),
                last_error
                    .as_deref()
                    .map_or_else(String::new, |msg| format!(": {msg}"))
            )));
        }
        if !failed_ids.is_empty() {
            tracing::warn!(
                title_id,
                succeeded = succeeded_ids.len(),
                failed = failed_ids.len(),
                cancelled = cancelled_mid_loop,
                "event-based replay unlock partially succeeded"
            );
        }
        if cancelled_mid_loop {
            tracing::info!(
                title_id,
                succeeded = succeeded_ids.len(),
                "event-based replay cancelled mid-batch — preserving credit for landed unlocks",
            );
        }
        return Ok(BatchOutcome {
            succeeded_ids,
            safety_skipped,
        });
    }

    // Non-event-based: single bulk POST covers up to 50 at a time,
    // `unlock_title_based_achievements` handles chunking internally.
    // Default chunk size = server max (64). `Some(1)` from caller =
    // strict per-game chronology (one POST per ach).
    let effective_chunk = chunk_size.unwrap_or(crate::xbox::constants::UNLOCK_CHUNK_SIZE);
    if scid_hint.is_empty() {
        // No SCID in the plan row → discover it by unlocking `ids[0]`
        // via the per-ID fast path (it caches the resolved SCID). The
        // remaining IDs then go through the chunked bulk POST.
        //
        // Critical detail: if the tail chunk fails after `ids[0]`
        // already landed on Xbox, we must NOT return `Err` with
        // "zero succeeded" — that loses credit for a real unlock
        // AND causes any retry to re-submit `ids[0]` (Xbox no-ops,
        // but our bookkeeping would double-count). Instead, return
        // an `Err` that CARRIES the partial success via the
        // outcome path, or — since the current caller treats `Err`
        // as "zero succeeded" — succeed-the-first-land and bubble
        // the tail error through `last_error` on the runner side.
        //
        // Simpler, robust approach: on tail failure, return
        // `Ok(BatchOutcome{succeeded_ids: vec![ids[0]]})` so the
        // first ID is credited. The tail IDs are not included in
        // succeeded_ids, which means the outer runner will see a
        // partial outcome and surface it through the partial-title
        // warning path (see Bug #3 fix).
        let title_id_t = crate::types::TitleId::try_from(title_id)
            .map_err(|e| Error::invalid_argument(format!("title_id `{title_id}`: {e}")))?;
        let xuid_t = crate::types::Xuid::try_from(xuid)
            .map_err(|e| Error::invalid_argument(format!("xuid `{xuid}`: {e}")))?;
        let ach0_t = crate::types::AchievementId::try_from(ids[0].as_str())
            .map_err(|e| Error::invalid_argument(format!("achievement_id `{}`: {e}", ids[0])))?;
        client
            .unlock_achievement(&title_id_t, &xuid_t, &ach0_t)
            .await?;
        if ids.len() > 1 {
            let scid = match client.resolve_service_config_id(xuid, title_id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        title_id,
                        ?e,
                        "scid resolve failed after partial unlock; crediting ids[0] and surfacing partial"
                    );
                    return Ok(BatchOutcome {
                        succeeded_ids: vec![ids[0].clone()],
                        safety_skipped,
                    });
                }
            };
            if let Err(e) = client
                .unlock_title_based_achievements_chunked(
                    &scid,
                    title_id,
                    xuid,
                    &ids[1..],
                    effective_chunk,
                )
                .await
            {
                tracing::warn!(
                    title_id,
                    ?e,
                    "tail chunk failed after ids[0] landed; crediting ids[0] only"
                );
                return Ok(BatchOutcome {
                    succeeded_ids: vec![ids[0].clone()],
                    safety_skipped,
                });
            }
        }
    } else {
        client
            .unlock_title_based_achievements_chunked(
                scid_hint,
                title_id,
                xuid,
                ids,
                effective_chunk,
            )
            .await?;
    }
    Ok(BatchOutcome {
        succeeded_ids: ids.to_vec(),
        safety_skipped,
    })
}

fn pacing_between(
    prev_ts: &Option<String>,
    this_ts: &str,
    is_first: bool,
    config: &ReplayConfig,
) -> Duration {
    if is_first {
        return Duration::ZERO;
    }
    if config.mirror_real_pacing {
        if let Some(prev) = prev_ts {
            let prev_s = parse_iso(prev);
            let this_s = parse_iso(this_ts);
            if let (Some(a), Some(b)) = (prev_s, this_s) {
                if b >= a {
                    let gap = Duration::from_secs((b - a) as u64);
                    // Apply floor only if the caller opted in (non-zero).
                    let floored = if config.min_delay > Duration::ZERO && gap < config.min_delay {
                        config.min_delay
                    } else {
                        gap
                    };
                    // Apply ceiling only if the caller opted in (non-zero).
                    // Zero = "no cap, mirror the real gap exactly" — this is
                    // the user-visible contract: a 10-minute gap in the
                    // source replays as a 10-minute wait here.
                    return if config.max_delay > Duration::ZERO && floored > config.max_delay {
                        config.max_delay
                    } else {
                        floored
                    };
                }
            }
        }
        // Mirror mode asked-for but timestamps unparseable / out of order:
        // fall through to fixed `delay_between` rather than firing instantly.
    }
    config.delay_between
}

/// Minimal ISO-8601 parser for the shape Xbox Live returns:
/// `YYYY-MM-DDThh:mm:ss(.fff)?Z`. Returns seconds since Unix epoch.
/// Re-exported wrapper — other modules in the plan crate
/// (`bit_by_bit`, for example) need the same ISO parser that drives
/// `mirror_real_pacing`. Kept as a thin `pub` alias so the core
/// implementation stays file-local.
#[must_use]
pub fn parse_iso_public(s: &str) -> Option<i64> {
    parse_iso(s)
}

/// Parse the same Xbox ISO-8601 shape as `parse_iso_public` but
/// preserve the **sub-second** fraction as a separate nanosecond
/// component.
///
/// Returns `(unix_seconds, sub_seconds_nanos)`. For the
/// non-fractional shape (`...:ssZ`) the nanos component is `0`.
///
/// **Why:** the bit-by-bit replay against a real-world 240k-entry
/// aggregate plan contains many same-second bursts (e.g. legacy 360
/// titles where Xbox awarded several achievements within a single
/// second). The previous `par_sort_unstable_by_key` keyed on the
/// truncated `i64` seconds shuffled those entries into an arbitrary
/// (but deterministic) order, breaking the replay's "exact submission
/// order" guarantee. Callers can now key sorts on `(secs, nanos)` for
/// a stable, chronologically correct order even within sub-seconds.
#[must_use]
pub fn parse_iso_subsec_public(s: &str) -> Option<(i64, u32)> {
    let secs = parse_iso(s)?;
    Some((secs, parse_iso_nanos(s)))
}

/// Extract the sub-second fraction from an ISO timestamp as
/// nanoseconds. Returns 0 when the string has no fractional part or
/// the fraction can't be parsed (we never want to stall the sort on
/// a single malformed entry).
fn parse_iso_nanos(s: &str) -> u32 {
    let s = s.trim();
    let after_dot = match s.split_once('.') {
        Some((_, after)) => after,
        None => return 0,
    };
    // Strip the trailing `Z` and any timezone offset suffix.
    let frac = after_dot
        .trim_end_matches('Z')
        .split(['+', '-'])
        .next()
        .unwrap_or("");
    if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    // Pad / truncate to exactly 9 digits (nanoseconds).
    let mut buf = [b'0'; 9];
    let take = frac.len().min(9);
    buf[..take].copy_from_slice(&frac.as_bytes()[..take]);
    let s = std::str::from_utf8(&buf).unwrap_or("0");
    s.parse::<u32>().unwrap_or(0)
}

/// Parse a numeric timezone offset suffix into MINUTES east of UTC.
/// Accepts `+HH:MM`, `-HH:MM`, `+HHMM`, `-HHMM`, `+HH`, `-HH`. Returns
/// `None` for any malformed input so the caller can decline to
/// adjust. Wave-5 audit clock-F1.
fn parse_iso_offset_minutes(off: &str) -> Option<i64> {
    let mut chars = off.chars();
    let sign: i64 = match chars.next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let rest: String = chars.collect();
    let cleaned: String = rest.chars().filter(|c| *c != ':').collect();
    // Wave-9 audit panic-W9-P1: previously the `&cleaned[..2]` /
    // `&cleaned[2..]` byte-slices panicked when `cleaned` contained
    // a non-ASCII char that landed mid-codepoint at byte index 2.
    // A community-supplied / hand-edited plan with a malformed
    // `time_unlocked` like `"...T12:34:56+\u{1F600}"` (any 4-byte
    // emoji after the sign) would crash the runner. The colon-
    // filter doesn't enforce digit-only — it only strips ':'. Fix:
    // reject anything that isn't ASCII digits before slicing. The
    // ASCII guard makes byte-indexing safe AND surfaces malformed
    // input as a clean None rather than a panic.
    if !cleaned.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h_str, m_str): (&str, &str) = match cleaned.len() {
        4 => (&cleaned[..2], &cleaned[2..]),
        2 => (&cleaned[..2], "00"),
        _ => return None,
    };
    let h: i64 = h_str.parse().ok()?;
    let m: i64 = m_str.parse().ok()?;
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    Some(sign * (h * 60 + m))
}

fn parse_iso(s: &str) -> Option<i64> {
    // BUG HUNTER M-parse-iso-trim: hand-edited plan files often
    // arrive with leading/trailing whitespace inside the JSON
    // string ("\t2024-01-01T00:00:00Z"). The old parser returned
    // None for those (the leading tab broke `splitn` on the year),
    // and the entry sorted to `i64::MIN` at the front of the
    // chronological order — which then triggered downstream
    // pacing math against a sentinel-positioned entry. Trim the
    // input first so well-formed-but-padded timestamps parse
    // correctly.
    let s = s.trim();
    // BUG HUNTER M4: reject the Xbox "never unlocked" sentinel
    // `0001-01-01T00:00:00(.fff...)?Z` at the top of the parser.
    // The old behaviour trimmed `Z` + split on `.` and happily
    // returned ~-62_135_596_800 for the sentinel — a real i64
    // timestamp that would sneak past callers who didn't also
    // `starts_with("0001-01-01")` check the raw string. The
    // builder filters it via `starts_with` before feeding into
    // `time_unlocked` (builder.rs:928), but `parse_iso_public` is
    // re-exported and consumed by `bit_by_bit::compute_schedule_*`
    // and `mirror_real_pacing` code paths that don't have the
    // prefix check — producing wildly-past scheduled gaps. Fail
    // early with None, matching the spirit of the sentinel ("no
    // valid unlock timestamp").
    if s.starts_with("0001-01-01") {
        return None;
    }
    let s = s.trim_end_matches('Z');
    let s = s.split('.').next()?;
    let (date, time_with_offset) = s.split_once('T')?;
    // BUG HUNTER tz-offset (wave-5 clock-F1): if the timestamp ends
    // in `+HH:MM` or `-HH:MM` (any offset other than `Z`), the old
    // parser's `tp.next()?.parse::<u32>()` for seconds choked on
    // `"00+05:30"`, returning `None`. The downstream scheduler then
    // silently dropped the entry from the chronological chain,
    // collapsing subsequent timing onto the previous anchor. Now we
    // detect a trailing offset on the TIME portion (the date can
    // contain '-' so we only scan past 'T'), strip + parse it, and
    // subtract from the UTC seconds total. Local-time-with-offset
    // → UTC is `local - offset`.
    let (time, offset_min) = match time_with_offset.rfind(['+', '-']) {
        Some(idx) => {
            let off_part = &time_with_offset[idx..];
            match parse_iso_offset_minutes(off_part) {
                Some(min) => (&time_with_offset[..idx], min),
                // Garbage offset → ignore the offset and parse the
                // whole tail as time (will fail cleanly via the
                // `parse::<u32>` below).
                None => (time_with_offset, 0),
            }
        }
        None => (time_with_offset, 0),
    };
    let mut dp = date.splitn(3, '-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    let mut tp = time.splitn(3, ':');
    let h: u32 = tp.next()?.parse().ok()?;
    let mi: u32 = tp.next()?.parse().ok()?;
    let se: u32 = tp.next()?.parse().ok()?;
    let secs_local = unix_from_civil(y, mo, d, h, mi, se);
    Some(secs_local.saturating_sub(offset_min.saturating_mul(60)))
}

/// Path to the persistent replay-errors log.
///
/// Lives next to `auth.json`, `xauth.txt`, etc. in
/// `%USERPROFILE%/Documents/XAU/`. One JSON object per line
/// (append-only JSONL). Survives between runs so the user can `cat`
/// it after the replay to see which titles failed and why.
fn replay_errors_log_path() -> Option<std::path::PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(std::path::Path::to_path_buf))
        .map(|p| p.join("XAU").join("replay-errors.jsonl"))
}

/// Path to the per-achievement unlock-history log.
///
/// Sibling to `replay-errors.jsonl` — one JSON line per
/// achievement attempt with status (success / failed / skipped).
/// Useful for after-the-fact audits and for power-users who want a
/// CSV-able record of every unlock the runner made.
pub fn unlock_history_log_path() -> Option<std::path::PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(std::path::Path::to_path_buf))
        .map(|p| p.join("XAU").join("unlock-history.jsonl"))
}

/// Path to the cumulative "failures database": every game that could
/// NOT be replicated (event-based / code:49, token-expired, hard error,
/// discontinued), across ALL runs, in `%USERPROFILE%/Documents/XAU/
/// failed-titles.jsonl`. Append-only JSONL, one record per (title, run)
/// failure. This is the persistent registry of impossible games the
/// spec asks for — read it back grouped by `title_id` to see which
/// games keep failing and why, and (future) to pre-skip them.
#[must_use]
pub fn failed_titles_db_path() -> Option<std::path::PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(std::path::Path::to_path_buf))
        .map(|p| p.join("XAU").join("failed-titles.jsonl"))
}

/// Append one failure record to the cumulative failures database. Same
/// durability discipline as the other JSONL writers (sanitize + atomic
/// append + `sync_data`). Best-effort: a write error is returned but
/// callers log-and-continue — a missed failure-log line must never
/// abort a run.
pub fn append_failed_title_line(line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let Some(path) = failed_titles_db_path() else {
        return Err(std::io::Error::other("no home dir"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let safe_line = sanitize_jsonl_line(line);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut buf = String::with_capacity(safe_line.len() + 1);
    buf.push_str(&safe_line);
    buf.push('\n');
    f.write_all(buf.as_bytes())?;
    let _ = f.sync_data();
    Ok(())
}

/// One game in the failures database, grouped across all runs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FailedTitleSummary {
    pub title_id: String,
    pub game: String,
    /// Most recent failure reason (stable slug: event_based / unauthorized
    /// / error / discontinued).
    pub reason: String,
    /// How many failure records exist for this title (across runs).
    pub fail_count: usize,
    /// Distinct runs it failed in.
    pub run_count: usize,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

/// Read the cumulative failures database and group it by `title_id`,
/// newest-failure first. Tolerant of corrupt/partial lines (skips them).
/// Returns an empty vec when the file doesn't exist yet.
#[must_use]
pub fn read_failed_titles_grouped() -> Vec<FailedTitleSummary> {
    let Some(path) = failed_titles_db_path() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    group_failed_titles(&content)
}

/// Pure grouping of failures-DB JSONL content by `title_id`, newest
/// first. Split out from the file read so it's unit-testable.
#[must_use]
pub fn group_failed_titles(content: &str) -> Vec<FailedTitleSummary> {
    // title_id -> accumulator
    let mut by_title: std::collections::HashMap<String, FailedTitleSummary> =
        std::collections::HashMap::new();
    let mut runs_by_title: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let title_id = v.get("title_id").and_then(|x| x.as_str()).unwrap_or("");
        if title_id.is_empty() {
            continue;
        }
        let game = v
            .get("game")
            .and_then(|x| x.as_str())
            .unwrap_or("(unknown)")
            .to_string();
        let reason = v
            .get("reason")
            .and_then(|x| x.as_str())
            .unwrap_or("error")
            .to_string();
        let ts = v
            .get("ts_ms")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let run_id = v
            .get("run_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        runs_by_title
            .entry(title_id.to_string())
            .or_default()
            .insert(run_id);
        by_title
            .entry(title_id.to_string())
            .and_modify(|s| {
                s.fail_count += 1;
                if ts >= s.last_seen_ms {
                    s.last_seen_ms = ts;
                    // Keep the freshest game name + reason.
                    s.game = game.clone();
                    s.reason = reason.clone();
                }
                if ts < s.first_seen_ms {
                    s.first_seen_ms = ts;
                }
            })
            .or_insert(FailedTitleSummary {
                title_id: title_id.to_string(),
                game,
                reason,
                fail_count: 1,
                run_count: 0,
                first_seen_ms: ts,
                last_seen_ms: ts,
            });
    }
    let mut out: Vec<FailedTitleSummary> = by_title
        .into_values()
        .map(|mut s| {
            s.run_count = runs_by_title
                .get(&s.title_id)
                .map_or(0, std::collections::HashSet::len);
            s
        })
        .collect();
    // Newest failure first.
    out.sort_by(|a, b| b.last_seen_ms.cmp(&a.last_seen_ms));
    out
}

/// Title ids that are KNOWN-IMPOSSIBLE from the failures database, for
/// pre-skipping. Only the deterministic `event_based` reason qualifies:
/// a code:49 title ("not in the unlock bundle") ALWAYS fails, so there
/// is no point even firing the ~1s probe at it again. `error` can be
/// transient (network), `unauthorized` is token expiry, and
/// `discontinued` is filtered by its own path — none of those are
/// pre-skipped, so a one-off hiccup never permanently drops a title.
#[must_use]
pub fn known_impossible_title_ids() -> std::collections::HashSet<String> {
    read_failed_titles_grouped()
        .into_iter()
        .filter(|t| t.reason == "event_based")
        .map(|t| t.title_id)
        .collect()
}

/// Directory holding one self-contained JSONL log per bit-by-bit run:
/// `%USERPROFILE%/Documents/XAU/bbb-runs/`.
#[must_use]
pub fn bbb_runs_dir() -> Option<std::path::PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(std::path::Path::to_path_buf))
        .map(|p| p.join("XAU").join("bbb-runs"))
}

/// Path to the complete per-run bit-by-bit log for `run_id`. Each run
/// gets its own file (`bbb-run-<run_id>.jsonl`) containing a
/// `bbb_run_start` header, every `bbb_wire` unlock event, and a
/// `bbb_run_end` summary — a self-contained forensic record you can
/// open and read on its own, independent of the shared rolling history.
#[must_use]
pub fn bbb_run_log_path(run_id: &str) -> Option<std::path::PathBuf> {
    // `run_id` is minted internally (unix_ms-pid) so it's filename-safe,
    // but sanitize defensively against path traversal / separators.
    let safe: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    bbb_runs_dir().map(|d| d.join(format!("bbb-run-{safe}.jsonl")))
}

/// Append one JSON line to a run's dedicated log file. Same durability
/// discipline as [`append_history_line`] (sanitize + atomic append +
/// `sync_data`) but NO rotation — a single run's log is bounded by the
/// run itself, and the file is the user's forensic receipt for it.
pub(crate) fn append_bbb_run_line(run_id: &str, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let Some(path) = bbb_run_log_path(run_id) else {
        return Err(std::io::Error::other("no home dir"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let safe_line = sanitize_jsonl_line(line);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut buf = String::with_capacity(safe_line.len() + 1);
    buf.push_str(&safe_line);
    buf.push('\n');
    f.write_all(buf.as_bytes())?;
    let _ = f.sync_data();
    Ok(())
}

/// Larger rotation threshold than replay-errors.jsonl because
/// success entries dominate this log (typical: 95% success / 5%
/// failure on a healthy run). 50 MiB is ~250k typical lines.
const UNLOCK_HISTORY_ROTATE_BYTES: u64 = 50 * 1024 * 1024;

/// Maximum bytes (incl. trailing newline) per JSONL audit line.
/// Picked under the smallest cross-platform atomic-append boundary:
///   - Linux POSIX `O_APPEND`: kernel-side guaranteed atomic per
///     `write(2)` only up to `PIPE_BUF` (4096 bytes) for pipes;
///     for regular files the practical guarantee is one filesystem
///     page (4 KiB on most setups).
///   - Windows NTFS: cluster-size atomicity (typically 4 KiB).
/// Capping at 4000 bytes leaves headroom under 4 KiB so a line
/// crossing concurrent CLI+GUI appends can't tear or interleave
/// regardless of FS quirks. Wave-5 audit JSONL F1.
const JSONL_MAX_LINE_BYTES: usize = 4000;

/// Sanitize and bound a JSONL line before appending. Wave-5 audit
/// findings JSONL F1+F2:
///   - F1 (cross-process append interleave): if the line exceeds
///     the OS atomic-append boundary (~4 KiB on both POSIX and
///     NTFS), two concurrent appenders can interleave and corrupt
///     the file. We replace any oversize input with a short
///     synthetic "oversize_dropped" record so per-line size is
///     always safely under the atomic boundary. The forensic trail
///     is preserved (we record the original length and a 200-char
///     prefix for diagnosis).
///   - F2 (embedded-newline framing): JSONL parsers split on raw
///     `\n` — a literal newline anywhere inside the line would tear
///     the record across two physical lines. `serde_json` already
///     escapes newlines inside strings, but a future caller might
///     hand-build the line with `format!`. Guard with a runtime
///     replacement so framing is robust to that mistake too.
/// Public wrapper so `safety::append_skip_audit` can reuse the
/// same sanitisation contract — JSONL atomicity guarantees should
/// hold uniformly across all forensic-log writers.
#[must_use]
pub fn sanitize_jsonl_line_public(line: &str) -> String {
    sanitize_jsonl_line(line)
}

fn sanitize_jsonl_line(line: &str) -> String {
    // Wave-8 audit resource-F2: previously this function ran THREE
    // sequential `.replace()` passes (each allocates a fresh String
    // of input.len() + escape-overhead bytes) BEFORE the size cap
    // engaged. A 100MB hostile input transiently allocated ~1GB to
    // produce a 4 KiB synthetic record. Cheap fix: size-check the
    // RAW input first; only run the three replaces on inputs we
    // already know are below the cap. The synthetic record's
    // 200-char prefix is taken from raw input via `chars()` (codepoint
    // safe), which is fine even for ill-formed escape sequences.
    if line.len() + 1 > JSONL_MAX_LINE_BYTES {
        return oversize_jsonl_record(line);
    }
    // Defensively fold embedded raw newlines + carriage returns into
    // their JSON escapes so one record stays on one physical line.
    //
    // CRITICAL: do NOT escape backslashes here. Every caller feeds an
    // ALREADY-serialized `serde_json` line, whose internal quotes are
    // valid `\"` escapes. The old code ran `.replace('\\', "\\\\")`
    // first, which turned each `\"` into `\\"` — an escaped backslash
    // followed by a BARE quote that terminates the JSON string early,
    // corrupting every wire line whose `error` field contained quotes
    // (all Xbox 4xx bodies do: `{"code":49,...}`). That silently broke
    // the machine-auditable evidence trail. Raw 0x0A/0x0D bytes are
    // distinct from the backslash-n sequences serde emits, so mapping
    // only those two is lossless for valid JSON and still sanitizes a
    // stray hand-built `format!` line.
    let normalized = if line.contains('\n') || line.contains('\r') {
        line.replace('\n', "\\n").replace('\r', "\\r")
    } else {
        line.to_string()
    };
    // After escaping, the line might have grown past the cap (each
    // `\n` doubles to two chars, each `\\` doubles to two). Re-check
    // and emit the synthetic record if so — the replace overhead is
    // still bounded (≤ 2× input) so the transient peak is fine.
    if normalized.len() + 1 <= JSONL_MAX_LINE_BYTES {
        return normalized;
    }
    oversize_jsonl_record(line)
}

/// Build the synthetic `{"kind":"oversize_dropped",...}` record for
/// a JSONL line that exceeds `JSONL_MAX_LINE_BYTES`. The prefix is a
/// codepoint-safe 200-char slice of the RAW input (escaped via
/// `serde_json::to_string` so any embedded quotes / backslashes /
/// control chars are JSON-encoded correctly).
fn oversize_jsonl_record(raw_line: &str) -> String {
    let prefix: String = raw_line.chars().take(200).collect();
    let prefix_json =
        serde_json::to_string(&prefix).unwrap_or_else(|_| "\"<unencodable>\"".to_string());
    format!(
        r#"{{"kind":"oversize_dropped","ts":"{}","original_len":{},"prefix":{}}}"#,
        iso_now_simple(),
        raw_line.len(),
        prefix_json,
    )
}

/// Append a single JSON line to the per-achievement history log.
/// Same rotation strategy as `append_error_line` (rename to `.old`
/// when oversized) but with a larger budget. Errors are non-fatal
/// — losing a log line is better than aborting the unlock.
pub(crate) fn append_history_line(line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let Some(path) = unlock_history_log_path() else {
        return Err(std::io::Error::other("no home dir"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // BUG HUNTER audit-rotation-cross-process (#11): rotate to a
    // unique-suffix archive instead of clobbering `.old`. Two xau
    // processes (CLI + GUI) crossing the threshold simultaneously
    // would otherwise both rename to `.old` and the second wins,
    // erasing one full rotation of audit lines.
    crate::plan::safety::rotate_audit_log_if_needed(&path, UNLOCK_HISTORY_ROTATE_BYTES);
    // BUG HUNTER M-jsonl-atomic-append: previously this was two
    // separate `write_all` calls (line + "\n"). On a process kill
    // between them, the file ended in a half-line of JSON; the next
    // append concatenated onto that half-line, producing a corrupt
    // record that broke JSONL parsers downstream. Combine the
    // newline into a single buffer so the append is one syscall —
    // POSIX/NTFS both make small append writes atomic up to
    // PIPE_BUF / cluster-size. Wave-5 added bounded line size +
    // newline-escape guard via `sanitize_jsonl_line`.
    let safe_line = sanitize_jsonl_line(line);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut buf = String::with_capacity(safe_line.len() + 1);
    buf.push_str(&safe_line);
    buf.push('\n');
    f.write_all(buf.as_bytes())?;
    // BUG HUNTER jsonl-fsync (wave-5 F3): without `sync_data` the
    // write lives in the page cache and a host power loss within
    // the kernel writeback window (~5s Linux, ~30s Windows) drops
    // it despite `write_all` returning Ok. Cheap (~1-3 ms on SSD)
    // and matches the `save_plan` durability contract.
    let _ = f.sync_data();
    Ok(())
}

/// When the error log grows past this threshold, rotate it to
/// `replay-errors.jsonl.old` (overwriting any existing `.old`) and
/// start fresh. 10 MiB is ~50k typical error lines — well beyond
/// anything a user would read interactively.
const REPLAY_ERRORS_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Append a single JSON line to the persistent replay-errors log.
/// Creates the file + parent dir if missing. Errors bubble up but
/// callers should treat them as non-fatal.
///
/// Rotates the file when it exceeds `REPLAY_ERRORS_ROTATE_BYTES`
/// (10 MiB) — moves the current file to `replay-errors.jsonl.old`
/// (clobbering any previous `.old`) and starts fresh. Single-file
/// rotation keeps disk use bounded without burdening the user with
/// a pile of timestamped archives.
fn append_error_line(line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let Some(path) = replay_errors_log_path() else {
        return Err(std::io::Error::other("no home dir"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // BUG HUNTER audit-rotation-cross-process (#11): see
    // append_history_line. Rotate to a unique-suffix archive so two
    // xau processes can't clobber each other's `.old`.
    crate::plan::safety::rotate_audit_log_if_needed(&path, REPLAY_ERRORS_ROTATE_BYTES);
    // BUG HUNTER M-jsonl-atomic-append + wave-5 JSONL F1+F2+F3:
    // sanitise (bound size + escape embedded newlines) and
    // sync_data so the line is durable, atomic-appendable, and
    // never tears its JSONL framing.
    let safe_line = sanitize_jsonl_line(line);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut buf = String::with_capacity(safe_line.len() + 1);
    buf.push_str(&safe_line);
    buf.push('\n');
    f.write_all(buf.as_bytes())?;
    let _ = f.sync_data();
    Ok(())
}

/// Minimal ISO-8601 timestamp for the error log entries.
/// Public wrapper so other plan submodules (notably `bit_by_bit`'s
/// discontinued-title audit) can stamp audit lines with the same
/// civil-from-unix shape the rest of the audit log uses.
#[must_use]
pub fn iso_now_simple_public() -> String {
    iso_now_simple()
}

fn iso_now_simple() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Use the same civil-from-unix math the builder uses; no chrono dep.
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let h = sod / 3600;
    let mi = (sod % 3600) / 60;
    let s = sod % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn unix_from_civil(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    // Inverse of civil_from_unix.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    days * 86_400 + h as i64 * 3600 + mi as i64 * 60 + s as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // -------- BBB-Timing-1: sub-second tiebreak in sort key ---------

    /// Wave-9 audit panic-W9-P1 + test-coverage HIGH-5: lock the
    /// timezone-offset parser, including the panic-on-emoji fix. The
    /// previous helper byte-sliced `&cleaned[..2]` without verifying
    /// `cleaned` was ASCII; a community-supplied plan with a
    /// `time_unlocked` ending in a 4-byte emoji after the sign would
    /// crash the runner with `byte index 2 is not a char boundary`.
    #[test]
    fn parse_iso_offset_minutes_accepts_canonical_forms() {
        assert_eq!(super::parse_iso_offset_minutes("+05:30"), Some(330));
        assert_eq!(super::parse_iso_offset_minutes("-08:00"), Some(-480));
        assert_eq!(super::parse_iso_offset_minutes("+0530"), Some(330));
        assert_eq!(super::parse_iso_offset_minutes("-0800"), Some(-480));
        assert_eq!(super::parse_iso_offset_minutes("+05"), Some(300));
        assert_eq!(super::parse_iso_offset_minutes("-08"), Some(-480));
        assert_eq!(super::parse_iso_offset_minutes("+00:00"), Some(0));
        assert_eq!(super::parse_iso_offset_minutes("-00:00"), Some(0));
    }

    #[test]
    fn parse_iso_offset_minutes_rejects_malformed_without_panic() {
        // Out-of-range hours / minutes.
        assert_eq!(super::parse_iso_offset_minutes("+24:00"), None);
        assert_eq!(super::parse_iso_offset_minutes("+05:60"), None);
        // Missing sign.
        assert_eq!(super::parse_iso_offset_minutes("05:30"), None);
        // Empty / sign-only.
        assert_eq!(super::parse_iso_offset_minutes(""), None);
        assert_eq!(super::parse_iso_offset_minutes("+"), None);
        assert_eq!(super::parse_iso_offset_minutes("-"), None);
        // Non-digit garbage.
        assert_eq!(super::parse_iso_offset_minutes("+abc"), None);
        assert_eq!(super::parse_iso_offset_minutes("+ab:cd"), None);
        // The smoking gun — non-ASCII codepoints in offset position.
        // Without the wave-9 fix, this panicked on `&cleaned[..2]`
        // because `\u{1F600}` (😀) is 4 bytes but a single char.
        assert_eq!(super::parse_iso_offset_minutes("+\u{1F600}"), None);
        assert_eq!(
            super::parse_iso_offset_minutes("+05:\u{1F600}\u{1F600}"),
            None
        );
        assert_eq!(super::parse_iso_offset_minutes("+\u{00E9}\u{00E9}"), None);
    }

    /// Wave-9 audit test-coverage HIGH-1: lock the oversize-record
    /// shape from `sanitize_jsonl_line`. Inputs > 4000 bytes are
    /// replaced with a synthetic `{"kind":"oversize_dropped",...}`
    /// record carrying the original length and a 200-char prefix.
    /// Wave-8 added the F1+F2 fix (size-check before .replace());
    /// the test was missed in that iter. Without this regression
    /// test, the synthetic-record format could drift silently.
    #[test]
    fn sanitize_jsonl_line_oversize_emits_synthetic_record() {
        let big = "x".repeat(8000);
        let out = super::sanitize_jsonl_line_public(&big);
        assert!(
            out.starts_with(r#"{"kind":"oversize_dropped""#),
            "oversize input must produce kind=oversize_dropped record, got: {}",
            &out[..out.len().min(200)],
        );
        assert!(
            out.contains(r#""original_len":8000"#),
            "synthetic record should preserve original_len"
        );
        // The synthetic record itself must be under the cap (the
        // whole point of the fix is to never emit a >4000-byte line).
        assert!(
            out.len() + 1 <= super::JSONL_MAX_LINE_BYTES,
            "synthetic record must be under JSONL_MAX_LINE_BYTES (got {})",
            out.len()
        );
        // Contains a `prefix` field.
        assert!(
            out.contains(r#""prefix""#),
            "synthetic record missing prefix field"
        );
    }

    #[test]
    fn sanitize_jsonl_line_passthrough_small_input() {
        // Small valid JSON line stays valid (no escape mangling).
        let small = r#"{"foo":"bar","count":42}"#;
        let out = super::sanitize_jsonl_line_public(small);
        assert_eq!(out, small, "small valid JSON must round-trip unchanged");
    }

    #[test]
    fn sanitize_jsonl_line_escapes_embedded_newlines() {
        // Hand-built `format!` could carry embedded raw `\n`/`\r`. The
        // sanitiser must fold them into their two-char JSON escapes so
        // JSONL framing isn't torn — WITHOUT touching backslashes that
        // are already part of valid escapes.
        let raw = "line1\nline2\rline3";
        let out = super::sanitize_jsonl_line_public(raw);
        assert!(!out.contains('\n'), "raw newlines must be escaped");
        assert!(!out.contains('\r'), "raw carriage returns must be escaped");
        assert!(out.contains("\\n"));
        assert!(out.contains("\\r"));
    }

    #[test]
    fn sanitize_jsonl_line_preserves_serde_quote_escapes() {
        // REGRESSION (replica-fidelity audit): a real wire line carries
        // an `error` field holding an Xbox 4xx body full of quotes,
        // which serde escapes as `\"`. The sanitiser must NOT double
        // the backslash (that produced `\\"` — a bare quote that ends
        // the string early and made EVERY failed-unlock line in the run
        // log unparseable). The line must round-trip through a JSON
        // parser byte-for-byte.
        let line = serde_json::json!({
            "kind": "bbb_wire",
            "status": "failed",
            "error": "HTTP status 400 Bad Request: {\"code\":49,\"source\":\"Progress\"}",
        })
        .to_string();
        let out = super::sanitize_jsonl_line_public(&line);
        assert_eq!(
            out, line,
            "already-valid serde JSON must pass through byte-identical"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("sanitised line must still parse as JSON");
        assert_eq!(
            parsed["error"],
            "HTTP status 400 Bad Request: {\"code\":49,\"source\":\"Progress\"}"
        );
    }

    #[test]
    fn group_failed_titles_dedups_counts_and_sorts_newest_first() {
        // Apex failed in two different runs; Farmer once. A junk line +
        // a line missing title_id are skipped. Newest failure first.
        let content = [
            r#"{"kind":"failed_title","title_id":"111","game":"Apex","reason":"event_based","run_id":"r1","ts_ms":1000}"#,
            r#"not json at all"#,
            r#"{"kind":"failed_title","title_id":"222","game":"Farmer","reason":"event_based","run_id":"r1","ts_ms":1500}"#,
            r#"{"title_id":"","game":"Nameless","reason":"error","ts_ms":9}"#,
            r#"{"kind":"failed_title","title_id":"111","game":"Apex Legends","reason":"unauthorized","run_id":"r2","ts_ms":3000}"#,
        ]
        .join("\n");
        let out = super::group_failed_titles(&content);
        assert_eq!(out.len(), 2, "two distinct titles (junk + no-id skipped)");
        // Apex is newest (ts 3000) → first, count 2 across 2 runs, and
        // the freshest name + reason win.
        assert_eq!(out[0].title_id, "111");
        assert_eq!(out[0].fail_count, 2);
        assert_eq!(out[0].run_count, 2);
        assert_eq!(out[0].game, "Apex Legends");
        assert_eq!(out[0].reason, "unauthorized");
        assert_eq!(out[0].first_seen_ms, 1000);
        assert_eq!(out[0].last_seen_ms, 3000);
        // Farmer second (ts 1500), single failure.
        assert_eq!(out[1].title_id, "222");
        assert_eq!(out[1].fail_count, 1);
        assert_eq!(out[1].run_count, 1);
        // Empty content → empty.
        assert!(super::group_failed_titles("").is_empty());
    }

    /// Wave-9: end-to-end check that an ISO timestamp with a TZ
    /// offset suffix parses correctly as UTC seconds. Was returning
    /// `None` before wave-5 (silently dropped from the chronology
    /// chain), now returns the proper UTC timestamp.
    #[test]
    fn parse_iso_handles_timezone_offsets() {
        // 12:00 in +05:30 is 06:30 UTC.
        let utc_z = super::parse_iso("2026-04-28T06:30:00Z").expect("baseline Z parse");
        let with_offset =
            super::parse_iso("2026-04-28T12:00:00+05:30").expect("offset form should parse");
        assert_eq!(
            utc_z, with_offset,
            "+05:30 offset should map to UTC equivalent"
        );
        // 12:00 in -08:00 is 20:00 UTC.
        let utc_z_neg = super::parse_iso("2026-04-28T20:00:00Z").expect("baseline");
        let with_neg =
            super::parse_iso("2026-04-28T12:00:00-08:00").expect("negative offset should parse");
        assert_eq!(utc_z_neg, with_neg);
    }

    #[test]
    fn parse_iso_subsec_extracts_microsecond_fraction() {
        // Real-world Xbox shape from the 240k-aggregate plan.
        let (secs, nanos) = parse_iso_subsec_public("2013-11-22T19:08:25.7375435Z").expect("parse");
        // 7,375,435 microseconds → 737_543_500 ns (truncated to 9 digits).
        assert_eq!(nanos, 737_543_500, "microsecond fraction → nanos");
        assert!(secs > 1_385_000_000 && secs < 1_385_200_000, "2013 unix");
    }

    #[test]
    fn parse_iso_subsec_zero_nanos_for_no_fraction() {
        let (_, nanos) = parse_iso_subsec_public("2024-01-01T00:00:00Z").expect("parse");
        assert_eq!(nanos, 0);
    }

    #[test]
    fn parse_iso_subsec_orders_same_second_burst_correctly() {
        // Sample-second burst that USED to shuffle under the old
        // truncated-i64 sort key. Now a stable composite sort keeps
        // the chronological order.
        let mut entries = vec![
            "2013-11-23T09:38:49.4195244Z",
            "2013-11-23T09:38:49.1000000Z",
            "2013-11-23T09:38:49.9999999Z",
            "2013-11-23T09:38:49.5012186Z",
        ];
        entries.sort_by_key(|s| parse_iso_subsec_public(s).unwrap_or((i64::MIN, 0)));
        assert_eq!(
            entries,
            vec![
                "2013-11-23T09:38:49.1000000Z",
                "2013-11-23T09:38:49.4195244Z",
                "2013-11-23T09:38:49.5012186Z",
                "2013-11-23T09:38:49.9999999Z",
            ],
            "sub-second sort must reproduce strict chronology"
        );
    }

    #[test]
    fn parse_iso_subsec_handles_truncation_to_9_digits() {
        // Anything beyond 9 fractional digits is silently truncated
        // (nanosecond precision is the floor). The truncation MUST
        // be a stable prefix — pad with zeros, not `parse::<u32>`-bomb.
        let (_, nanos) =
            parse_iso_subsec_public("2024-01-01T00:00:00.123456789012Z").expect("parse");
        assert_eq!(nanos, 123_456_789);
    }

    #[test]
    fn parse_iso_subsec_rejects_garbage_fraction_gracefully() {
        // A non-digit fraction yields nanos=0 instead of failing the
        // whole parse — the timestamp still sorts correctly to the
        // second, just without the bonus precision.
        let (_, nanos) = parse_iso_subsec_public("2024-01-01T00:00:00.abcZ").expect("parse");
        assert_eq!(nanos, 0);
    }

    #[test]
    fn parse_iso_subsec_returns_none_for_sentinel() {
        // The Xbox "never unlocked" sentinel must propagate as None
        // so the caller can decide (currently → exclude from sort).
        assert!(parse_iso_subsec_public("0001-01-01T00:00:00Z").is_none());
    }

    // -------- credit_gamerscore -------------------------------------

    fn write_test_events_bundle(
        title_id: &str,
        achievement_id: &str,
    ) -> Arc<crate::events::EventsBundle> {
        let dir = std::env::temp_dir().join(format!(
            "xau-runner-events-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp events dir");
        let data = format!(
            r#"{{
                "{title_id}": {{
                    "FullySupported": true,
                    "Achievements": {{
                        "{achievement_id}": {{}}
                    }}
                }}
            }}"#
        );
        std::fs::write(dir.join("Data.json"), data).expect("write Data.json");
        let bundle = crate::events::EventsBundle::load(&dir).expect("load bundle");
        Arc::new(bundle)
    }

    fn sample_plan() -> Vec<(String, i32)> {
        vec![
            ("ach_a".into(), 10),
            ("ach_b".into(), 30),
            ("ach_c".into(), 50),
            ("ach_d".into(), 100),
        ]
    }

    /// Regression guard: the previous impl summed the FIRST N items of
    /// the plan when it saw N successes — so an events-path outcome of
    /// {ach_b, ach_d} (skipping a and c) would be credited as ach_a +
    /// ach_b. Semantically wrong. The new impl must use set membership.
    #[test]
    fn credit_gamerscore_is_set_membership_not_prefix() {
        let ach_list = sample_plan();
        // Server accepted ach_b and ach_d (out of order, sparse).
        let succeeded = vec!["ach_b".into(), "ach_d".into()];
        assert_eq!(credit_gamerscore(&ach_list, &succeeded), 30 + 100);
        // Server accepted nothing.
        assert_eq!(credit_gamerscore(&ach_list, &[]), 0);
        // Server accepted all.
        let all: Vec<String> = ach_list.iter().map(|(id, _)| id.clone()).collect();
        assert_eq!(credit_gamerscore(&ach_list, &all), 10 + 30 + 50 + 100);
    }

    /// An unknown ID in `succeeded_ids` (server-side telemetry ID that
    /// doesn't match anything in the plan) must NOT contribute
    /// gamerscore. Otherwise a server bug could inflate the user's
    /// score counter.
    #[test]
    fn credit_gamerscore_ignores_unknown_ids() {
        let ach_list = sample_plan();
        let succeeded = vec!["ghost_id".into(), "ach_a".into()];
        assert_eq!(credit_gamerscore(&ach_list, &succeeded), 10);
    }

    /// Duplicates in `succeeded_ids` must not double-count.
    #[test]
    fn credit_gamerscore_duplicate_succeeds_does_not_double_count() {
        let ach_list = sample_plan();
        let succeeded = vec!["ach_c".into(), "ach_c".into(), "ach_c".into()];
        assert_eq!(credit_gamerscore(&ach_list, &succeeded), 50);
    }

    /// Regression: `unlock_title_batch(&[])` must not panic — empty
    /// batches shouldn't flow through the bulk runner in practice but
    /// the function is a soft boundary and needs to be safe.
    #[tokio::test]
    async fn unlock_title_batch_empty_ids_returns_ok_empty() {
        use crate::xbox::client::XboxClientOptions;
        let client = XboxClient::new(XboxClientOptions {
            xauth: "XBL3.0 x=1;dummy".into(),
            ..Default::default()
        })
        .expect("client");
        let out = unlock_title_batch(
            &client,
            "123",
            "",
            "1",
            &[],
            None,
            crate::plan::safety::SafetyPolicy::strict(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("empty batch should be Ok");
        assert!(out.succeeded_ids.is_empty());
    }

    #[tokio::test]
    async fn unlock_title_batch_event_based_all_failed_returns_err() {
        use crate::xbox::client::XboxClientOptions;

        let bundle = write_test_events_bundle("123", "ach1");
        let client = XboxClient::new(XboxClientOptions {
            xauth: "XBL3.0 x=1;dummy".into(),
            events_bundle: Some(bundle),
            ..Default::default()
        })
        .expect("client");

        let result = unlock_title_batch(
            &client,
            "123",
            "",
            "1",
            &[String::from("ach1")],
            None,
            crate::plan::safety::SafetyPolicy::danger_allow_unsafe(),
            Arc::new(AtomicBool::new(false)),
        )
        .await;

        match result {
            Ok(_) => panic!("all-failed event path must not look successful"),
            Err(err) => assert!(err
                .to_string()
                .contains("event-based unlock failed for all")),
        }
    }

    #[test]
    fn parse_iso_roundtrip() {
        assert_eq!(parse_iso("2024-01-01T00:00:00Z"), Some(1_704_067_200));
        assert_eq!(parse_iso("2026-04-21T09:05:56Z"), Some(1_776_762_356));
        assert_eq!(
            parse_iso("2024-06-15T12:34:56.789Z"),
            Some(parse_iso("2024-06-15T12:34:56Z").unwrap())
        );
    }

    #[test]
    fn parse_iso_rejects_garbage() {
        assert_eq!(parse_iso(""), None);
        assert_eq!(parse_iso("not-a-date"), None);
        assert_eq!(parse_iso("2024/01/01"), None);
    }

    #[test]
    fn pacing_mirrors_real_gap_exactly_by_default() {
        // Default mirror config: no min, no max — exact mirror.
        let config = ReplayConfig {
            mirror_real_pacing: true,
            ..Default::default()
        };
        // 30-second gap in source → 30-second wait in replay.
        let d = pacing_between(
            &Some("2024-01-01T00:00:00Z".into()),
            "2024-01-01T00:00:30Z",
            false,
            &config,
        );
        assert_eq!(d, Duration::from_secs(30));

        // 10-MINUTE gap in source → 10-minute wait in replay (NOT clamped).
        // This is the regression guard: the old default clamped this to 5 min.
        let d = pacing_between(
            &Some("2024-01-01T00:00:00Z".into()),
            "2024-01-01T00:10:00Z",
            false,
            &config,
        );
        assert_eq!(d, Duration::from_secs(600));

        // 1-hour gap also preserved exactly.
        let d = pacing_between(
            &Some("2024-01-01T00:00:00Z".into()),
            "2024-01-01T01:00:00Z",
            false,
            &config,
        );
        assert_eq!(d, Duration::from_secs(3600));

        // Same-second unlocks → zero wait (no floor by default).
        let d = pacing_between(
            &Some("2024-01-01T00:00:00Z".into()),
            "2024-01-01T00:00:00Z",
            false,
            &config,
        );
        assert_eq!(d, Duration::ZERO);
    }

    #[test]
    fn pacing_respects_explicit_clamp_when_opted_in() {
        // Caller opts in to safety rails.
        let config = ReplayConfig {
            mirror_real_pacing: true,
            min_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(60),
            ..Default::default()
        };

        // 30s gap passes through unchanged.
        let d = pacing_between(
            &Some("2024-01-01T00:00:00Z".into()),
            "2024-01-01T00:00:30Z",
            false,
            &config,
        );
        assert_eq!(d, Duration::from_secs(30));

        // 1-hour gap hits the 60s ceiling.
        let d = pacing_between(
            &Some("2024-01-01T00:00:00Z".into()),
            "2024-01-01T01:00:00Z",
            false,
            &config,
        );
        assert_eq!(d, Duration::from_secs(60));

        // 1-second gap hits the 5s floor.
        let d = pacing_between(
            &Some("2024-01-01T00:00:00Z".into()),
            "2024-01-01T00:00:01Z",
            false,
            &config,
        );
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn pacing_first_entry_is_zero() {
        let d = pacing_between(
            &None,
            "2024-01-01T00:00:00Z",
            true,
            &ReplayConfig::default(),
        );
        assert_eq!(d, Duration::ZERO);
    }

    #[test]
    fn pacing_unparseable_timestamps_falls_back_to_delay_between() {
        let config = ReplayConfig {
            mirror_real_pacing: true,
            delay_between: Duration::from_secs(42),
            ..Default::default()
        };
        // Garbage timestamps — mirror can't do its thing, so we use the
        // fixed delay rather than firing instantly.
        let d = pacing_between(&Some("not-a-date".into()), "also-bad", false, &config);
        assert_eq!(d, Duration::from_secs(42));
    }
}
