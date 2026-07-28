// Module-level allow list:
// - `needless_update`: tests use `..Default::default()` on fully-
//   specified struct literals so adding a field to `PlanEntry`
//   doesn't break every test at once.
#![allow(clippy::needless_update)]

//! Pure-function estimator for bit-by-bit replays.
//!
//! Given a plan, the caller's current unlocks, and a bit-by-bit
//! config (selected titles, gap mode, time budget), compute without
//! touching the network:
//!
//! - how many achievements would land NOW
//! - how many would land within `time_budget`
//! - total gamerscore of each bucket
//! - per-title breakdown for UI listing
//! - when the next batch outside the budget would fire
//!
//! Used by the GUI's bit-by-bit panel for a "preview" card that
//! updates live as the user toggles titles / gap mode / time budget.
//! Since it's pure, the GUI can call it every keystroke without
//! worrying about rate limits.
//!
//! The per-entry math is the SAME function the live runner uses
//! (`compute_schedule_for_title`) — so what the preview says WILL
//! happen matches what actually happens, byte for byte. That's the
//! whole point of fixing audit bug #12 (scheduler deduplication).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::bit_by_bit::{
    compute_schedule_for_title_with_speedup, compute_tiered_schedule_with_speedup, BitByBitConfig,
    GapMode,
};
use crate::plan::Plan;

/// Per-title preview of what bit-by-bit would do with the given
/// config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleEstimate {
    pub title_id: String,
    pub game_name: String,
    /// First-unlock → last-unlock span in the source target plan
    /// (seconds). 0 if the title has <2 entries or timestamps don't
    /// parse. Used for the "shortest-first" ranking.
    pub span_seconds: i64,
    /// Total achievements the target has for this title in the plan.
    pub total_plan_entries: usize,
    /// Achievements the caller already has.
    pub already_done: usize,
    /// Achievements that would fire this run (scheduled within the
    /// eligibility horizon: `[now, now + time_budget]`).
    pub eligible_count: usize,
    /// Gamerscore of the eligible set.
    pub eligible_gs: i32,
    /// True when the plan's entry count matches the titlehub total —
    /// i.e. the target 100%'d this title. UI uses this to surface
    /// the "fastest 100%" ranking.
    pub is_100_percent: bool,
    /// 0-based position in the RUNNER'S dispatch order.
    ///
    /// `per_title` itself is re-sorted for display (by eligible
    /// gamerscore, so the biggest wins head the preview table), which
    /// is deliberately NOT the order titles are processed in. Anything
    /// claiming to show "what runs next" has to sort by this instead —
    /// the Status page's queue did not, and silently listed the
    /// preview's order while calling it the dispatch order.
    #[serde(default)]
    pub dispatch_rank: usize,
}

/// Full estimate result. Totals AND per-title breakdown; the UI
/// can render both a summary line and a sortable table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitByBitEstimate {
    /// Achievements eligible RIGHT NOW (scheduled <= now, regardless
    /// of time_budget).
    pub unlockable_now: usize,
    pub gamerscore_now: i32,
    /// Achievements eligible within `[now, now + time_budget]`. When
    /// `time_budget` is None these equal `*_now`.
    pub unlockable_within_budget: usize,
    pub gamerscore_within_budget: i32,
    /// The earliest scheduled time STRICTLY outside the budget
    /// window. When `time_budget = None` this is the earliest
    /// future entry. `None` when nothing is pending.
    pub earliest_outside_budget_unix: Option<i64>,
    /// Titles that contributed at least one eligible entry.
    pub titles_touched: usize,
    pub per_title: Vec<TitleEstimate>,
}

/// Bulk-scan snapshot of the caller's own unlocks, indexed by title.
/// The GUI caches this (TTL 5 min) via `plan_cache_my_unlocks`; we
/// take it as a reference to avoid a redundant copy.
pub type MyUnlocksIndex = HashMap<String, (HashSet<String>, Option<i64>)>;

/// One time-window of a deterministic forecast.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ForecastWindow {
    /// Human label (`"1h"`, `"24h"`, `"30d"`, `"complete"`).
    pub label: String,
    /// Window length in seconds; `None` = "complete" (whole plan).
    pub duration_secs: Option<u64>,
    /// Achievements the scheduler will fire within `[now, now+window]`.
    pub unlocks_in_window: usize,
    /// Gamerscore of that set.
    pub gamerscore_in_window: i32,
    /// Subset eligible RIGHT NOW (scheduled ≤ now).
    pub unlocks_eligible_now: usize,
    pub gamerscore_eligible_now: i32,
    /// Distinct titles contributing at least one in-window unlock.
    pub titles_touched: usize,
    /// Earliest scheduled time strictly outside the window, if any.
    pub earliest_outside_window_unix: Option<i64>,
}

/// **Deterministic** forecast across several time windows — the REAL
/// prediction of what the scheduler will fire, NOT an extrapolation of
/// observed unlocks/second.
///
/// For each window it runs [`estimate`] with that window as the time
/// budget, using the caller's `config` (gap_mode, max_speedup, top_n,
/// catch_up, …) verbatim so the projection matches exactly what a live
/// run with those settings would dispatch. `"complete"` windows (a
/// `None` duration) use a 1000-year budget so the estimator returns the
/// full asymptote instead of collapsing to "eligible right now".
///
/// Pure: no I/O, no network. The caller (CLI `bbb-forecast`, GUI ETA
/// cards) supplies the current `my_unlocks` snapshot so already-done
/// entries drop out of the projection as the run progresses.
#[must_use]
pub fn forecast_windows(
    plan: &Plan,
    my_unlocks_by_title: &MyUnlocksIndex,
    config: &BitByBitConfig,
    windows: &[(String, Option<u64>)],
    now_unix: i64,
) -> Vec<ForecastWindow> {
    // A 1000-year budget stands in for "complete" — far past any real
    // plan's span, so the estimator returns every entry it would ever
    // fire rather than only what's due now.
    const COMPLETE_BUDGET_SECS: u64 = 1000 * 365 * 86400;
    // Physical dispatch ceiling: the runner paces unlocks to Xbox's
    // budget (see `pace_unlock`), so a window of N seconds can land at
    // most `N * REQUESTS / (PERIOD_MS/1000)` unlocks — no matter how many
    // the schedule marks eligible. Without this the +1h card claims tens
    // of thousands of unlocks that physically cannot fire in an hour
    // (the user-observed 67k/+1h vs a ~24k/hr ceiling).
    let period_secs = super::bit_by_bit::UNLOCK_RATE_LIMIT_PERIOD_MS / 1000;
    windows
        .iter()
        .map(|(label, secs)| {
            let budget = Duration::from_secs(secs.unwrap_or(COMPLETE_BUDGET_SECS));
            // Apply the cap via `max_entries` so estimate()'s dispatch_cap
            // truncates BOTH unlocks_in_window AND gamerscore_in_window
            // consistently. Finite windows only — "complete" is uncapped.
            let mut cfg = config.clone();
            // SEQUENTIAL: skip the rate ceiling entirely. It models the
            // concurrent path's real bottleneck (Xbox's 100 req/15 s
            // budget), but a sequential run paces ONE game across hours
            // and never approaches it — applying the cap here would
            // silently truncate a forecast whose true limit is the sum
            // of the playtime floors, already modelled in `estimate`.
            if let Some(s) = secs.filter(|_| !config.sequential) {
                let dispatchable = s
                    .saturating_mul(super::bit_by_bit::UNLOCK_RATE_LIMIT_REQUESTS)
                    .saturating_div(period_secs.max(1));
                let cap = usize::try_from(dispatchable).unwrap_or(usize::MAX);
                cfg.max_entries = if config.max_entries == 0 {
                    cap
                } else {
                    config.max_entries.min(cap)
                };
            }
            let est = estimate(
                plan,
                my_unlocks_by_title,
                &cfg,
                Some(Some(budget)),
                now_unix,
            );
            ForecastWindow {
                label: label.clone(),
                duration_secs: *secs,
                unlocks_in_window: est.unlockable_within_budget,
                gamerscore_in_window: est.gamerscore_within_budget,
                unlocks_eligible_now: est.unlockable_now,
                gamerscore_eligible_now: est.gamerscore_now,
                titles_touched: est.titles_touched,
                earliest_outside_window_unix: est.earliest_outside_budget_unix,
            }
        })
        .collect()
}

/// Compute the estimate. Pure: deterministic, no I/O, safe to call
/// every keystroke.
///
/// - `plan` is the source plan (target's entries + title_totals).
/// - `my_unlocks_by_title` is the caller's current unlocks grouped
///   by title. Empty HashMap = caller has nothing.
/// - `config` drives filters (selected_titles, top_n, gap_mode).
/// - `time_budget_override` optional override of `config.time_budget`
///   — GUI preview sweeps budgets without mutating config.
/// - `now_unix` is the "treat as now" timestamp (so tests can freeze
///   time).
#[must_use]
pub fn estimate(
    plan: &Plan,
    my_unlocks_by_title: &MyUnlocksIndex,
    config: &BitByBitConfig,
    time_budget_override: Option<Option<Duration>>,
    now_unix: i64,
) -> BitByBitEstimate {
    // Explicit override wins; otherwise fall back to config.
    let budget = time_budget_override.unwrap_or(config.time_budget);
    let horizon = match budget {
        Some(d) => now_unix.saturating_add(i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
        None => now_unix,
    };

    // Group plan entries by title_id while preserving order.
    //
    // BUG HUNTER EST-N1: dedupe by (title_id, ach_id) before
    // bucketing. The live bit-by-bit runner (`bit_by_bit.rs:596`)
    // dedupes defensively so corrupted plans don't send duplicate
    // POSTs. The estimator must apply the same dedupe so its
    // "will unlock N / X G" numbers match what the runner will
    // actually dispatch — otherwise a plan with internal duplicates
    // would show inflated projections while the live run fires
    // fewer unlocks.
    let mut seen_keys: HashSet<(String, String)> = HashSet::with_capacity(plan.entries.len());
    let mut by_title: HashMap<String, Vec<&crate::plan::PlanEntry>> = HashMap::new();
    for e in &plan.entries {
        if !seen_keys.insert((e.title_id.clone(), e.achievement_id.clone())) {
            continue;
        }
        by_title.entry(e.title_id.clone()).or_default().push(e);
    }

    // Apply selection filter (explicit pick wins over top_n).
    let selected_ids: Option<&HashSet<String>> =
        config.selected_titles.as_ref().filter(|s| !s.is_empty());

    // Build per-title preview candidates. We clone PlanEntries for
    // the scheduler because it takes &[PlanEntry] — cloning is cheap
    // (strings inside are short) and keeps the estimate function
    // self-contained.
    let mut previews: Vec<(String, String, Vec<crate::plan::PlanEntry>, bool)> = Vec::new();
    for (tid, entries) in &by_title {
        if let Some(sel) = selected_ids {
            if !sel.contains(tid) {
                continue;
            }
        }
        let total_expected = plan
            .title_totals
            .get(tid)
            .map_or(0, |tc| tc.total_achievements);
        // BUG HUNTER bbb-topn-over100: `>=` to match the canonical
        // completion predicate and the runner's top-N filter — an
        // over-100% title (more unique entries than titlehub's stale
        // total) is still 100%-complete.
        let is_100_percent =
            total_expected > 0 && usize::try_from(total_expected).is_ok_and(|t| entries.len() >= t);
        let game_name = entries
            .first()
            .map(|e| e.game_name.clone())
            .unwrap_or_default();
        let owned: Vec<crate::plan::PlanEntry> = entries.iter().map(|&e| e.clone()).collect();
        previews.push((tid.clone(), game_name, owned, is_100_percent));
    }

    // Sort previews deterministically BEFORE applying top-N filter or
    // dispatch cap. `run()` at `bit_by_bit.rs:561-568` sorts title_plans
    // unconditionally; if we only sort inside the `top_n_titles > 0`
    // branch, the HashMap-iteration order (varies per-process via
    // `RandomState`) leaks into the dispatch-cap truncation when
    // `max_entries > 0` + `top_n_titles == 0` or `selected_titles = Some`.
    // That makes preview non-deterministic across keystrokes AND
    // diverges from `run()`'s actual dispatch. Audit: Agent-3 finding
    // #10. Module doc at :91-92 explicitly advertises "Pure:
    // deterministic, no I/O, safe to call every keystroke".
    previews.sort_by_cached_key(|(tid, _, entries, _)| {
        let span =
            crate::plan::compute_span_seconds(entries.iter().map(|e| e.time_unlocked.as_str()));
        let gamerscore: i64 = entries
            .iter()
            .map(|e| i64::from(e.gamerscore.unwrap_or(0)))
            .sum();
        // MUST mirror `run()`'s sort key exactly (rate DESC, duration,
        // count, tid) — preview and live run share one ranking, and the
        // Status page's "up next" is this list.
        //
        // `duration` is the REAL completion time (playtime when the
        // plan carries it, capped calendar-span guess otherwise), not
        // the raw span: a title 100%'d over a weekend has a short span
        // even when it cost 40 hours, so ranking by span put the long
        // games first. See `Plan::effective_completion_secs`.
        let duration = plan.effective_completion_secs(tid, span);
        (
            std::cmp::Reverse(crate::plan::compute_gs_rate_milli(gamerscore, duration)),
            duration,
            entries.len(),
            tid.clone(),
        )
    });
    // Apply top_n_titles (same contract as run()): a pure truncate of
    // the fastest-G-rate ranking above. USER CONTRACT (2026-07-13):
    // no 100% filter, no minimum entry count — non-100% titles count.
    if config.top_n_titles > 0 && selected_ids.is_none() && previews.len() > config.top_n_titles {
        previews.truncate(config.top_n_titles);
    }

    // Shadow-replay slot promotion — mirror `run()` at
    // `bit_by_bit.rs:1427` (`apply_shadow_slot_promotion`). When
    // `active_slots_cap` is set (every shadow preset: psicota=8,
    // new_account=6, max_speed=8), the runner drops every "not started"
    // title (user has 0 unlocks) beyond `cap - in_progress_count` from
    // the run. If the estimator skips this, its preview counts entries
    // from titles the live run will never touch — inflating
    // `unlockable_now`/`within_budget` by potentially hundreds of
    // entries and breaking the module's "matches what actually happens"
    // contract. Runs AFTER the rate sort + top-N truncation so the
    // retained set (fastest-G-rate-first) is identical to the runner's.
    if config.active_slots_cap > 0 {
        let cap = config.active_slots_cap;
        let user_set = |tid: &str| my_unlocks_by_title.get(tid).map(|(set, _)| set);
        let has_pending = |tid: &str, entries: &[crate::plan::PlanEntry]| {
            user_set(tid).is_some_and(|set| {
                !set.is_empty() && entries.iter().any(|e| !set.contains(&e.achievement_id))
            })
        };
        let in_progress = previews
            .iter()
            .filter(|(tid, _, entries, _)| has_pending(tid, entries))
            .count();
        let available_slots = cap.saturating_sub(in_progress);
        let mut kept_not_started = 0usize;
        previews.retain(|(tid, _, _, _)| {
            let has_unlocks = user_set(tid).is_some_and(|set| !set.is_empty());
            if has_unlocks {
                return true;
            }
            if kept_not_started < available_slots {
                kept_not_started += 1;
                true
            } else {
                false
            }
        });
    }

    // Score each title through the shared scheduler.
    let mut out = BitByBitEstimate {
        unlockable_now: 0,
        gamerscore_now: 0,
        unlockable_within_budget: 0,
        gamerscore_within_budget: 0,
        earliest_outside_budget_unix: None,
        titles_touched: 0,
        per_title: Vec::with_capacity(previews.len()),
    };

    // Global dispatch cap that mirrors `run()`'s `max_entries`
    // semantics: once we've counted `cap` entries that WOULD be
    // dispatched (i.e. scheduled within the horizon), stop counting.
    // Without this the preview card said "500 would land" while the
    // real start() ran with max_entries=20 and only dispatched 20.
    // Audit bug BBB-Estimate-Limit#2.
    let dispatch_cap = if config.max_entries == 0 {
        usize::MAX
    } else {
        config.max_entries
    };

    // ============ SEQUENTIAL MODE: a DIFFERENT algorithm ============
    //
    // In sequential mode the runner does NOT follow the gap schedule
    // computed below. `run()` marks EVERY not-already-done entry of a
    // game eligible (`config.sequential || scheduled <= horizon`) and
    // paces the whole game across its own playtime floor, N games in
    // flight. So the binding constraint is the SUM OF FLOORS — not the
    // source's dormant gaps, and not the request-rate ceiling that
    // `forecast_windows` clamps by (one game at a time never gets
    // within an order of magnitude of Xbox's budget).
    //
    // Scoring a sequential run against the gap schedule is scoring the
    // wrong algorithm, and it is wrong in BOTH directions: it
    // under-counts a game whose 20 achievements the target earned over
    // three days (the runner fires all 20 inside one floor), and
    // over-counts when many titles look simultaneously eligible even
    // though only `sequential_window` of them can actually run.
    //
    // So: simulate the real dispatch. Lanes = `sequential_window`;
    // each game occupies its lane for `floor` seconds (already
    // compressed by `max_speedup`), and its k-th of n unlocks lands at
    // `start + floor*(k+1)/n` — the same arithmetic as
    // `floor_adjusted_target`. (The runner's `floor_adjusted_targets`
    // additionally reshapes the spacing INSIDE that window to match the
    // target's cadence; it cannot move an unlock outside
    // `[start, start+floor]`, so window totals are unaffected and only
    // the ordering within a partially-covered game can differ.)
    let seq_land: HashMap<String, Vec<i64>> = if config.sequential {
        // Same ceiling the dispatcher clamps to — via the constant, so
        // the forecast can't silently model more lanes than the runner
        // will ever open if that ceiling is ever revised.
        let lanes = config
            .sequential_window
            .clamp(1, super::bit_by_bit::MAX_SIMULTANEOUS_TITLES);
        // Lane free-times in seconds relative to `now_unix`.
        let mut lane_free: Vec<i64> = vec![0; lanes];
        let mut map: HashMap<String, Vec<i64>> = HashMap::with_capacity(previews.len());
        for (tid, _, entries, _) in &previews {
            let done: usize = my_unlocks_by_title
                .get(tid)
                .map(|(set, _)| {
                    entries
                        .iter()
                        .filter(|e| set.contains(&e.achievement_id))
                        .count()
                })
                .unwrap_or(0);
            let n = entries.len().saturating_sub(done);
            if n == 0 {
                continue;
            }
            // Same floor the dispatcher computes: real MinutesPlayed
            // when the plan carries it, else the calendar span; floored
            // at `min_game_seconds`, then divided by the speedup tier.
            // Mirrors the dispatcher exactly: real MinutesPlayed is
            // uncapped, a calendar-span guess is bounded (see
            // `FLOOR_GUESS_CAP_SECS`). Without the cap this forecast
            // faithfully reported "0 unlocks in 30 days" — correct
            // about a runner that would have taken years per title.
            let real = plan
                .title_min_completion_secs
                .get(tid)
                .copied()
                .filter(|&s| s > 0)
                .and_then(|s| u64::try_from(s).ok());
            let base = match real {
                Some(secs) => secs,
                None => super::bit_by_bit::cap_guessed_floor_secs(
                    u64::try_from(crate::plan::compute_span_seconds(
                        entries.iter().map(|e| e.time_unlocked.as_str()),
                    ))
                    .unwrap_or(0),
                ),
            };
            let raw = base.max(config.min_game_seconds).max(1);
            let floor = i64::try_from(super::bit_by_bit::floor_speedup_secs(
                raw,
                config.max_speedup,
                config.min_game_seconds,
            ))
            .unwrap_or(i64::MAX / 2);
            // Earliest-free lane wins, mirroring the windowed join.
            let li = lane_free
                .iter()
                .enumerate()
                .min_by_key(|(_, t)| **t)
                .map_or(0, |(i, _)| i);
            let start = lane_free[li];
            lane_free[li] = start.saturating_add(floor);
            let n_i = i64::try_from(n).unwrap_or(1).max(1);
            map.insert(
                tid.clone(),
                (0..n)
                    .map(|k| {
                        let step = i64::try_from(k).unwrap_or(0).saturating_add(1);
                        now_unix
                            .saturating_add(start)
                            .saturating_add(floor.saturating_mul(step) / n_i)
                    })
                    .collect(),
            );
        }
        map
    } else {
        HashMap::new()
    };

    // `previews` is in DISPATCH order at this point; the index is the
    // runner's position, captured before the display re-sort below
    // scrambles it.
    for (dispatch_rank, (title_id, game_name, entries, is_100_percent)) in
        previews.into_iter().enumerate()
    {
        // Cap check at the TOP of the title loop, not mid-entry. A
        // mid-entry `break 'title_loop` used to skip the per-title
        // push AND `titles_touched += 1` for the title whose entries
        // had already been counted into the global rollups — preview
        // reported "1 eligible within budget across 0 titles" while
        // `run()` actually dispatched 1 unlock on that title. Audit:
        // Agent-3 finding #9.
        if out.unlockable_within_budget >= dispatch_cap {
            break;
        }
        let (user_unlocked, user_last_unix) = my_unlocks_by_title
            .get(&title_id)
            .cloned()
            .unwrap_or_default();
        // Dispatch to the same scheduler `run()` picks at
        // `bit_by_bit.rs:788`. The estimator has no
        // `user_first_unix` (the my-unlocks cache doesn't carry
        // per-unlock timestamps), so we pass None for Tiered, and
        // `user_last_unix` (when available) for classic. Both
        // schedulers clamp their anchor to `max(historical,
        // now_unix)` — see the anchor-clamp comments in
        // `compute_schedule_for_title_with_speedup` (line ~3631)
        // and `compute_tiered_schedule_with_speedup` (line ~3887).
        // The clamp was added to fix the preview/runner divergence
        // flagged by Agent-1 + Agent-3 + Agent-4: the runner used
        // to receive the historical timestamp and produce past-due
        // schedules, while the estimator anchored at NOW — they
        // now converge because both fold to NOW whenever the
        // historical value lies in the past.
        let schedule = match &config.gap_mode {
            GapMode::Tiered(tiers) => compute_tiered_schedule_with_speedup(
                &entries,
                None,
                &user_unlocked,
                tiers,
                now_unix,
                config.max_speedup,
            ),
            _ => compute_schedule_for_title_with_speedup(
                &entries,
                user_last_unix,
                &user_unlocked,
                config.gap_mode.clone(),
                now_unix,
                config.max_speedup,
            ),
        };
        // Mirror run()'s anti-burst deficit-shift so the preview counts
        // match what the runner will dispatch (CatchUpMode::Shift is the
        // default; Burst leaves overdue entries as-is).
        let mut schedule = schedule;
        if config.catch_up == super::bit_by_bit::CatchUpMode::Shift {
            super::bit_by_bit::apply_catch_up_shift(&mut schedule, now_unix);
        }

        let span =
            crate::plan::compute_span_seconds(entries.iter().map(|e| e.time_unlocked.as_str()));
        let already = schedule.iter().filter(|s| s.already_done).count();
        let mut title_eligible_count = 0usize;
        let mut title_eligible_gs: i32 = 0;
        // Sequential mode replaces the gap schedule's timing with the
        // floor simulation above; `not_done_idx` walks that title's
        // land-time vector in the same order the runner fires.
        let seq_times: Option<&Vec<i64>> = seq_land.get(&title_id);
        let mut not_done_idx = 0usize;
        for s in &schedule {
            if s.already_done {
                continue;
            }
            // Inner cap check: stop consuming entries of THIS title
            // but still fall through to the per-title push below so
            // the title's partial contribution (eligible_count,
            // eligible_gs) is visible in `per_title`.
            if out.unlockable_within_budget >= dispatch_cap {
                break;
            }
            // Effective fire time: the simulated floor pacing in
            // sequential mode, the gap schedule otherwise.
            let fire_at = match seq_times {
                Some(v) => v.get(not_done_idx).copied().unwrap_or(s.scheduled_unix),
                None => s.scheduled_unix,
            };
            not_done_idx += 1;
            if fire_at <= now_unix {
                out.unlockable_now += 1;
                out.gamerscore_now = out.gamerscore_now.saturating_add(s.gamerscore);
            }
            if fire_at <= horizon {
                out.unlockable_within_budget += 1;
                out.gamerscore_within_budget =
                    out.gamerscore_within_budget.saturating_add(s.gamerscore);
                title_eligible_count += 1;
                title_eligible_gs = title_eligible_gs.saturating_add(s.gamerscore);
            } else {
                // Outside the budget window — track earliest so UI
                // can say "next 12 achievements in 6h23m".
                out.earliest_outside_budget_unix = Some(
                    out.earliest_outside_budget_unix
                        .map_or(fire_at, |e| e.min(fire_at)),
                );
            }
        }
        if title_eligible_count > 0 {
            out.titles_touched += 1;
        }
        out.per_title.push(TitleEstimate {
            dispatch_rank,
            title_id,
            game_name,
            span_seconds: span,
            total_plan_entries: entries.len(),
            already_done: already,
            eligible_count: title_eligible_count,
            eligible_gs: title_eligible_gs,
            is_100_percent,
        });
    }

    // DISPLAY ordering: eligible_gs desc (biggest wins head the
    // preview table), then span asc, then title_id for stability.
    //
    // This is NOT the dispatch order — the runner goes fastest-first,
    // which is a different key entirely. Consumers that show "what
    // runs next" must sort by `dispatch_rank`; the field exists
    // precisely because this sort throws that order away.
    out.per_title.sort_by(|a, b| {
        b.eligible_gs
            .cmp(&a.eligible_gs)
            .then(a.span_seconds.cmp(&b.span_seconds))
            .then(a.title_id.cmp(&b.title_id))
    });

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::bit_by_bit::MaxSpeedup;
    use crate::plan::{PlanEntry, TitleCompletion};

    const NOW: i64 = 1_700_000_000;

    fn mk_entry(id: &str, title: &str, iso: &str, gs: i32) -> PlanEntry {
        PlanEntry {
            title_id: title.into(),
            service_config_id: "scid".into(),
            achievement_id: id.into(),
            achievement_name: format!("ach {id}"),
            game_name: format!("game {title}"),
            time_unlocked: iso.into(),
            gamerscore: Some(gs),
            ..Default::default()
        }
    }

    fn mk_plan(entries: Vec<PlanEntry>) -> Plan {
        let mut totals = HashMap::new();
        // Assume every title is 100% (set total = entry count per title).
        let mut counts = HashMap::new();
        for e in &entries {
            *counts.entry(e.title_id.clone()).or_insert(0) += 1i32;
        }
        for (tid, count) in counts {
            totals.insert(
                tid,
                TitleCompletion {
                    current_achievements: count,
                    total_achievements: count,
                },
            );
        }
        Plan {
            total: entries.len(),
            entries,
            title_totals: totals,
            ..Default::default()
        }
    }

    /// Field-reported: a live run sat at "1 / 96.666" with every
    /// forecast card reading +0G, and "~261d left" in the elapsed line.
    ///
    /// Cause: the plan carried no real `MinutesPlayed`, so each title's
    /// floor fell back to its CALENDAR SPAN. A title whose first and
    /// last unlock were three years apart became a 1095-day floor — the
    /// runner fired one achievement (probe-first), scheduled the next
    /// 438 days out, and the forecast correctly reported nothing would
    /// land for a month. The forecast was right; the floor was absurd.
    #[test]
    fn calendar_span_fallback_is_capped_so_a_run_can_progress() {
        let entries = vec![
            mk_entry("a1", "t1", "2021-01-01T00:00:00Z", 10),
            mk_entry("a2", "t1", "2021-06-01T00:00:00Z", 10),
            mk_entry("a3", "t1", "2022-06-01T00:00:00Z", 10),
            mk_entry("a4", "t1", "2023-06-01T00:00:00Z", 10),
            mk_entry("a5", "t1", "2024-01-01T00:00:00Z", 10),
        ];
        // NOTE: `title_min_completion_secs` deliberately EMPTY — this is
        // the "stat unavailable" path the cap exists for.
        let plan = mk_plan(entries);
        let raw_span = crate::plan::compute_span_seconds(
            plan.entries.iter().map(|e| e.time_unlocked.as_str()),
        );
        assert!(
            raw_span > 1000 * 86400,
            "fixture must span years to exercise the cap, got {raw_span}s"
        );

        let cfg = BitByBitConfig {
            sequential: true,
            ..Default::default()
        };
        let day = estimate(
            &plan,
            &HashMap::new(),
            &cfg,
            Some(Some(Duration::from_secs(86_400))),
            NOW,
        );
        assert_eq!(
            day.unlockable_within_budget, 5,
            "a 24h floor must complete this 5-achievement title within 24h; \
             uncapped it was 0 because the floor was 1095 days"
        );

        // A real MinutesPlayed is authoritative and must NOT be capped:
        // a genuine 40-hour completionist grind stays 40 hours.
        let mut plan_real = plan.clone();
        plan_real
            .title_min_completion_secs
            .insert("t1".into(), 40 * 3600);
        let day_real = estimate(
            &plan_real,
            &HashMap::new(),
            &cfg,
            Some(Some(Duration::from_secs(86_400))),
            NOW,
        );
        assert!(
            day_real.unlockable_within_budget < 5,
            "a real 40h floor must still outlast a 24h window, got {}",
            day_real.unlockable_within_budget
        );
    }

    /// The ranking exists in THREE copies — `run()`'s dispatch sort,
    /// `Plan::title_spans_fastest_gs_first` (GUI summary), and this
    /// estimator (preview, forecast, and the Status page's "up next").
    /// A comment saying "MUST mirror run()" did not stop them drifting:
    /// when the key moved from calendar span to real playtime, this
    /// copy was left behind, so the Status page would have listed a
    /// different order than the runner actually takes.
    ///
    /// This pins the two that are reachable from here against the same
    /// fixture. If they disagree, one of them was edited alone.
    #[test]
    fn estimator_ranking_agrees_with_the_shared_helper() {
        let mut plan = mk_plan(vec![
            // 40 hours of play, earned across 2 days.
            mk_entry("g1", "grinder", "2024-01-01T00:00:00Z", 500),
            mk_entry("g2", "grinder", "2024-01-03T00:00:00Z", 500),
            // 20 minutes of play, earned across 2 years.
            mk_entry("q1", "quickie", "2022-01-01T00:00:00Z", 500),
            mk_entry("q2", "quickie", "2024-01-01T00:00:00Z", 500),
        ]);
        plan.title_min_completion_secs
            .insert("grinder".into(), 40 * 3600);
        plan.title_min_completion_secs
            .insert("quickie".into(), 20 * 60);

        let shared: Vec<String> = plan
            .title_spans_fastest_gs_first()
            .into_iter()
            .map(|s| s.title_id)
            .collect();
        let cfg = BitByBitConfig::default();
        // `per_title` is display-sorted; `dispatch_rank` carries the
        // runner's order, which is what has to agree with the helper.
        let mut rows = estimate(&plan, &HashMap::new(), &cfg, None, NOW).per_title;
        rows.sort_by_key(|t| t.dispatch_rank);
        let estimated: Vec<String> = rows.into_iter().map(|t| t.title_id).collect();

        assert_eq!(
            estimated, shared,
            "estimator ranking drifted from the shared helper"
        );
        assert_eq!(
            estimated.first().map(String::as_str),
            Some("quickie"),
            "the 20-minute game must be first in BOTH"
        );
    }

    #[test]
    fn empty_plan_returns_zero() {
        let plan = mk_plan(vec![]);
        let cfg = BitByBitConfig::default();
        let got = estimate(&plan, &HashMap::new(), &cfg, None, NOW);
        assert_eq!(got.unlockable_now, 0);
        assert_eq!(got.unlockable_within_budget, 0);
        assert_eq!(got.titles_touched, 0);
        assert!(got.per_title.is_empty());
    }

    /// Sequential mode must be scored by the SUM OF PLAYTIME FLOORS,
    /// not by the gap schedule. Two 1-hour games, four achievements
    /// each, unlocked by the target a full day apart:
    ///
    /// - Concurrent model: the day-long gap dominates, so a 1-hour
    ///   budget sees almost nothing.
    /// - Sequential model (what actually runs): game A is completed
    ///   inside its own 1-hour floor — all four land — and game B has
    ///   not started yet.
    ///
    /// This divergence is the reported "estimate is imprecise": the
    /// preview was forecasting an algorithm the run does not use.
    #[test]
    fn sequential_estimate_uses_playtime_floors_not_gap_schedule() {
        const HOUR: i64 = 3600;
        let entries = vec![
            mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("a2", "t1", "2024-01-01T00:20:00Z", 10),
            mk_entry("a3", "t1", "2024-01-01T00:40:00Z", 10),
            mk_entry("a4", "t1", "2024-01-01T01:00:00Z", 10),
            // Second game a DAY later — huge gap for the concurrent model.
            mk_entry("b1", "t2", "2024-01-02T00:00:00Z", 10),
            mk_entry("b2", "t2", "2024-01-02T00:20:00Z", 10),
            mk_entry("b3", "t2", "2024-01-02T00:40:00Z", 10),
            mk_entry("b4", "t2", "2024-01-02T01:00:00Z", 10),
        ];
        let mut plan = mk_plan(entries);
        // Real MinutesPlayed floors: one hour each.
        plan.title_min_completion_secs.insert("t1".into(), HOUR);
        plan.title_min_completion_secs.insert("t2".into(), HOUR);

        let budget = Some(Some(Duration::from_secs(HOUR as u64)));
        let seq = |window: usize, tier: MaxSpeedup| {
            let cfg = BitByBitConfig {
                sequential: true,
                sequential_window: window,
                max_speedup: tier,
                ..Default::default()
            };
            estimate(&plan, &HashMap::new(), &cfg, budget, NOW).unlockable_within_budget
        };

        // ONE game at a time: game A fits its 1h floor exactly (its
        // last unlock lands at NOW+3600); game B has not begun.
        assert_eq!(
            seq(1, MaxSpeedup::Off),
            4,
            "one game completes in its floor"
        );
        // TWO at a time: both floors run in parallel, so both finish.
        assert_eq!(seq(2, MaxSpeedup::Off), 8, "the window doubles throughput");
        // Speedup halves each floor, so both games fit serially too.
        assert_eq!(
            seq(1, MaxSpeedup::Mild2x),
            8,
            "2x compresses both 1h floors into the 1h budget"
        );

        // THE DIVERGENCE, in the direction that actually bites. The
        // concurrent model anchors EVERY title's chain at now and runs
        // them in parallel, so the day between the two games is
        // irrelevant to it: it promises all 8 unlocks in the hour. A
        // real sequential run delivers 4 — one game, in its floor.
        // The old preview reported the 8.
        let concurrent = BitByBitConfig {
            sequential: false,
            gap_mode: GapMode::Exact,
            ..Default::default()
        };
        let conc =
            estimate(&plan, &HashMap::new(), &concurrent, budget, NOW).unlockable_within_budget;
        assert_eq!(conc, 8, "concurrent runs both chains from now");
        assert!(
            conc > seq(1, MaxSpeedup::Off),
            "the concurrent model over-promises for a sequential run \
             (concurrent={conc}, sequential=4) — that gap is the bug"
        );
    }

    #[test]
    fn all_already_done_returns_zero_eligible() {
        let entries = vec![
            mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("b", "t1", "2024-01-02T00:00:00Z", 20),
        ];
        let plan = mk_plan(entries);
        let mut my_unlocks: MyUnlocksIndex = HashMap::new();
        let mut set = HashSet::new();
        set.insert("a".to_string());
        set.insert("b".to_string());
        my_unlocks.insert("t1".into(), (set, Some(NOW - 3600)));
        let cfg = BitByBitConfig::default();
        let got = estimate(&plan, &my_unlocks, &cfg, None, NOW);
        assert_eq!(got.unlockable_now, 0);
        assert_eq!(got.gamerscore_now, 0);
        assert_eq!(got.per_title.len(), 1);
        assert_eq!(got.per_title[0].already_done, 2);
        assert_eq!(got.per_title[0].eligible_count, 0);
    }

    #[test]
    fn time_budget_6h_excludes_7h_schedules() {
        // Two entries in same title; gap = 7h.
        let entries = vec![
            mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("b", "t1", "2024-01-01T07:00:00Z", 20),
        ];
        let plan = mk_plan(entries);
        let cfg = BitByBitConfig {
            // Concurrent path: this test is ABOUT the gap schedule (sequential mode replaces it with playtime floors).
            sequential: false,
            gap_mode: GapMode::Exact,
            ..Default::default()
        };
        // User has NOTHING → first entry fires at NOW (user_last_unix=None),
        // so scheduling relative-to-running_user_last=NOW means:
        //   a fires at NOW
        //   b fires at NOW + 0 (first entry sets running to NOW) + 7h gap from source
        // Wait — first entry: no running_user_last → fires at NOW, and
        // updates running to NOW. Second entry: running=NOW, source_gap=7h,
        // scheduled = NOW + 7h.
        let got = estimate(
            &plan,
            &HashMap::new(),
            &cfg,
            Some(Some(Duration::from_secs(6 * 3600))),
            NOW,
        );
        assert_eq!(got.unlockable_now, 1, "only the 'a' fires at now");
        assert_eq!(
            got.unlockable_within_budget, 1,
            "'b' is 7h out, budget is 6h"
        );
        assert!(got.earliest_outside_budget_unix.is_some());
    }

    #[test]
    fn time_budget_8h_includes_7h_schedule() {
        let entries = vec![
            mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("b", "t1", "2024-01-01T07:00:00Z", 20),
        ];
        let plan = mk_plan(entries);
        let cfg = BitByBitConfig {
            gap_mode: GapMode::Exact,
            ..Default::default()
        };
        let got = estimate(
            &plan,
            &HashMap::new(),
            &cfg,
            Some(Some(Duration::from_secs(8 * 3600))),
            NOW,
        );
        assert_eq!(got.unlockable_within_budget, 2);
        assert_eq!(got.gamerscore_within_budget, 30);
    }

    #[test]
    fn scaled_mode_compresses_gaps_so_more_fits_in_budget() {
        // 10h source gap compressed by 0.1 = 1h → fits in 2h budget.
        let entries = vec![
            mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("b", "t1", "2024-01-01T10:00:00Z", 20),
        ];
        let plan = mk_plan(entries);
        let cfg = BitByBitConfig {
            // Concurrent path: this test is ABOUT the gap schedule (sequential mode replaces it with playtime floors).
            sequential: false,
            gap_mode: GapMode::Scaled(0.1),
            ..Default::default()
        };
        let got = estimate(
            &plan,
            &HashMap::new(),
            &cfg,
            Some(Some(Duration::from_secs(2 * 3600))),
            NOW,
        );
        assert_eq!(
            got.unlockable_within_budget, 2,
            "10h source × 0.1 = 1h, fits in 2h budget"
        );
    }

    #[test]
    fn selected_titles_narrows_scope() {
        let entries = vec![
            mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("b", "t2", "2024-01-01T00:00:00Z", 50),
        ];
        let plan = mk_plan(entries);
        let mut sel = HashSet::new();
        sel.insert("t1".to_string());
        let cfg = BitByBitConfig {
            // Concurrent path: this test is ABOUT the gap schedule (sequential mode replaces it with playtime floors).
            sequential: false,
            selected_titles: Some(sel),
            ..Default::default()
        };
        let got = estimate(&plan, &HashMap::new(), &cfg, None, NOW);
        assert_eq!(got.gamerscore_now, 10, "t2 filtered out by selection");
        assert_eq!(got.per_title.len(), 1);
    }

    #[test]
    fn top_n_picks_highest_gs_rate_even_if_not_100pct() {
        // USER CONTRACT (2026-07-13): "fastest" = gamerscore per
        // second of target wall-clock. t900 earned 900G over 20s
        // (45 G/s); t3000 earned 3000G over 21s (~143 G/s) — t3000
        // wins top_n = 1 even though its raw span is LONGER, and
        // even though the target has NOT 100%-completed it.
        let entries = vec![
            mk_entry("a", "t900", "2024-01-01T00:00:00Z", 450),
            mk_entry("b", "t900", "2024-01-01T00:00:20Z", 450),
            mk_entry("c", "t3000", "2024-01-01T00:00:00Z", 1500),
            mk_entry("d", "t3000", "2024-01-01T00:00:21Z", 1500),
        ];
        let mut plan = mk_plan(entries);
        // Mark t3000 as INCOMPLETE on target (2 of 50) — the old
        // ranking would have dropped it from top-N entirely; the
        // rate ranking must still pick it.
        plan.title_totals.insert(
            "t3000".to_string(),
            TitleCompletion {
                current_achievements: 2,
                total_achievements: 50,
            },
        );
        let cfg = BitByBitConfig {
            top_n_titles: 1,
            ..Default::default()
        };
        let got = estimate(&plan, &HashMap::new(), &cfg, None, NOW);
        assert_eq!(got.per_title.len(), 1);
        assert_eq!(
            got.per_title[0].title_id, "t3000",
            "3000G/21s (143 G/s) outranks 900G/20s (45 G/s)"
        );
    }

    #[test]
    fn top_n_includes_single_achievement_titles() {
        // Single-unlock titles have span 0, which clamps to 1s in
        // `compute_gs_rate_milli` — they rank by raw gamerscore. A
        // lone 500G unlock (500 G/s) outranks 100G over 20s (5 G/s).
        // The old ranking excluded 1-entry titles from top-N; the
        // rate ranking must include them.
        let entries = vec![
            mk_entry("a", "tslow", "2024-01-01T00:00:00Z", 50),
            mk_entry("b", "tslow", "2024-01-01T00:00:20Z", 50),
            mk_entry("c", "tburst", "2024-01-01T00:00:00Z", 500),
        ];
        let plan = mk_plan(entries);
        let cfg = BitByBitConfig {
            top_n_titles: 1,
            ..Default::default()
        };
        let got = estimate(&plan, &HashMap::new(), &cfg, None, NOW);
        assert_eq!(got.per_title.len(), 1);
        assert_eq!(
            got.per_title[0].title_id, "tburst",
            "single 500G burst outranks 5 G/s slow title"
        );
    }

    // ============================================================
    // Regression: Tiered gap_mode must dispatch to
    // `compute_tiered_schedule`, NOT fall through to the classic
    // `compute_schedule_for_title` anchored on user_last. Prior
    // behaviour: estimator always called classic, so preset runs
    // previewed with Capped-30d semantics while start() executed
    // with Tiered semantics — tens of thousands of gs divergence
    // on long-dormant target profiles. Audit: Agent-1/-3/-4 HIGH.
    // ============================================================

    #[test]
    fn tiered_gap_mode_is_dispatched_not_fallthrough() {
        // 10-hour source gap. With a very aggressive tier table
        // (first tier matches >=10h gap with speedup=100), the
        // scaled gap shrinks to 6 minutes — `b` fires almost
        // immediately even with a tiny time budget. If the
        // dispatcher ignored Tiered and fell through to
        // compute_schedule_for_title, apply() would STILL scale
        // the gap correctly (both paths delegate to GapMode::apply),
        // so the ELIGIBLE COUNT alone doesn't prove dispatch.
        //
        // What DOES prove dispatch: the anchor. Classic uses
        // user_last=NOW (because my_unlocks contains a timestamp),
        // tiered uses user_first (None → falls back to NOW). Same
        // anchor in this case, so the gs numbers converge — the
        // real proof is that the function runs to completion with
        // `GapMode::Tiered` without tripping the debug_assert in
        // `compute_schedule_for_title`. In debug builds, a missed
        // dispatch would panic here.
        let entries = vec![
            mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("b", "t1", "2024-01-01T10:00:00Z", 20),
        ];
        let plan = mk_plan(entries);
        let cfg = BitByBitConfig {
            // Concurrent path: this test is ABOUT the gap schedule (sequential mode replaces it with playtime floors).
            sequential: false,
            gap_mode: GapMode::Tiered(vec![
                (3600, 1.0),       // <= 1h: no compression
                (u64::MAX, 100.0), // everything else: 100x faster
            ]),
            ..Default::default()
        };
        let got = estimate(
            &plan,
            &HashMap::new(),
            &cfg,
            Some(Some(Duration::from_secs(30 * 60))), // 30-min budget
            NOW,
        );
        // 10h / 100 = 6 min, fits in 30-min budget → both entries
        // land.
        assert_eq!(
            got.unlockable_within_budget, 2,
            "Tiered must compress 10h to 6min — both entries fit in 30m budget"
        );
        assert_eq!(got.gamerscore_within_budget, 30);
    }

    #[test]
    fn forecast_windows_widen_monotonically_and_are_deterministic() {
        // One title, entries 1h apart over 3h. Wider windows capture
        // strictly more (or equal) unlocks — the deterministic forecast.
        let entries = vec![
            mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("b", "t1", "2024-01-01T01:00:00Z", 20),
            mk_entry("c", "t1", "2024-01-01T02:00:00Z", 30),
            mk_entry("d", "t1", "2024-01-01T03:00:00Z", 40),
        ];
        let plan = mk_plan(entries);
        let cfg = BitByBitConfig {
            gap_mode: GapMode::Exact,
            ..Default::default()
        };
        let windows = vec![
            ("1h".to_string(), Some(3600)),
            ("24h".to_string(), Some(86_400)),
            ("complete".to_string(), None),
        ];
        let f1 = forecast_windows(&plan, &HashMap::new(), &cfg, &windows, NOW);
        assert_eq!(f1.len(), 3);
        // Fresh account: 'a' fires now, then +1h/+2h/+3h. 1h window
        // catches a + b (b at +1h == horizon); complete catches all 4.
        assert!(f1[0].unlocks_in_window <= f1[1].unlocks_in_window);
        assert!(f1[1].unlocks_in_window <= f1[2].unlocks_in_window);
        assert_eq!(f1[2].unlocks_in_window, 4, "complete captures every entry");
        assert_eq!(f1[2].gamerscore_in_window, 100);
        // Deterministic: same inputs → identical output (no rate/time
        // dependence).
        let f2 = forecast_windows(&plan, &HashMap::new(), &cfg, &windows, NOW);
        assert_eq!(f1, f2);
    }

    #[test]
    fn forecast_caps_short_windows_by_throughput_ceiling() {
        // 40 achievements all unlockable ~now. A 1-second window can
        // physically dispatch at most 1 * 100/15 = 6 (integer), and a
        // 2-second window at most 13 — NOT all 40 — because the runner
        // paces to Xbox's budget. "complete" is uncapped (all 40).
        let entries: Vec<PlanEntry> = (0..40)
            .map(|i| mk_entry(&format!("a{i}"), "t1", "2024-01-01T00:00:00Z", 10))
            .collect();
        let plan = mk_plan(entries);
        let cfg = BitByBitConfig {
            // Concurrent path: this test is ABOUT the gap schedule (sequential mode replaces it with playtime floors).
            sequential: false,
            gap_mode: GapMode::Exact,
            ..Default::default()
        };
        let windows = vec![
            ("1s".to_string(), Some(1)),
            ("2s".to_string(), Some(2)),
            ("complete".to_string(), None),
        ];
        let f = super::forecast_windows(&plan, &HashMap::new(), &cfg, &windows, NOW);
        assert_eq!(f[0].unlocks_in_window, 6, "1s window caps at 1*100/15 = 6");
        assert_eq!(
            f[0].gamerscore_in_window, 60,
            "gamerscore capped in lockstep"
        );
        assert_eq!(
            f[1].unlocks_in_window, 13,
            "2s window caps at 2*100/15 = 13"
        );
        assert_eq!(f[2].unlocks_in_window, 40, "complete is uncapped");
    }

    #[test]
    fn active_slots_cap_drops_fresh_titles_beyond_cap() {
        // Three fresh titles (user has 0 unlocks), cap = 1. The runner
        // (apply_shadow_slot_promotion) keeps only 1 not-started title
        // — the first after the fastest-G-rate sort. The estimator
        // must match so its preview counts equal the run's. Rates:
        // t2 = 70G/1h ≈ 19 mG/s wins; t1 (30G/3d) and t3 (110G/2d)
        // both truncate to 0 mG/s and lose.
        let entries = vec![
            // t1: 3-day span
            mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("a2", "t1", "2024-01-04T00:00:00Z", 20),
            // t2: 1-hour span (highest G-rate → the one kept)
            mk_entry("b1", "t2", "2024-01-01T00:00:00Z", 30),
            mk_entry("b2", "t2", "2024-01-01T01:00:00Z", 40),
            // t3: 2-day span
            mk_entry("c1", "t3", "2024-01-01T00:00:00Z", 50),
            mk_entry("c2", "t3", "2024-01-03T00:00:00Z", 60),
        ];
        let plan = mk_plan(entries);
        let cfg = BitByBitConfig {
            gap_mode: GapMode::Exact,
            active_slots_cap: 1,
            ..Default::default()
        };
        let got = estimate(&plan, &HashMap::new(), &cfg, None, NOW);
        assert_eq!(
            got.per_title.len(),
            1,
            "cap=1 with no in-progress titles keeps exactly one fresh title"
        );
        assert_eq!(
            got.per_title[0].title_id, "t2",
            "the highest-G-rate fresh title is the one promoted"
        );
    }

    #[test]
    fn active_slots_cap_always_keeps_in_progress_titles() {
        // t1 is in-progress (user has 1 of 2 unlocks); t2, t3 fresh.
        // cap = 1 → in_progress=1, available_slots=0 → t1 kept, both
        // fresh titles dropped. Mirrors apply_shadow_slot_promotion.
        let entries = vec![
            mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("a2", "t1", "2024-01-02T00:00:00Z", 20),
            mk_entry("b1", "t2", "2024-01-01T00:00:00Z", 30),
            mk_entry("b2", "t2", "2024-01-01T01:00:00Z", 40),
            mk_entry("c1", "t3", "2024-01-01T00:00:00Z", 50),
            mk_entry("c2", "t3", "2024-01-03T00:00:00Z", 60),
        ];
        let plan = mk_plan(entries);
        let mut my_unlocks: MyUnlocksIndex = HashMap::new();
        let mut t1_set = HashSet::new();
        t1_set.insert("a1".to_string());
        my_unlocks.insert("t1".into(), (t1_set, Some(NOW - 3600)));
        let cfg = BitByBitConfig {
            gap_mode: GapMode::Exact,
            active_slots_cap: 1,
            ..Default::default()
        };
        let got = estimate(&plan, &my_unlocks, &cfg, None, NOW);
        let ids: Vec<&str> = got.per_title.iter().map(|t| t.title_id.as_str()).collect();
        assert_eq!(ids, ["t1"], "only the in-progress title survives cap=1");
    }

    #[test]
    fn active_slots_cap_zero_is_unbounded() {
        // cap = 0 must disable promotion entirely (all titles scored).
        let entries = vec![
            mk_entry("a1", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("a2", "t1", "2024-01-02T00:00:00Z", 20),
            mk_entry("b1", "t2", "2024-01-01T00:00:00Z", 30),
            mk_entry("b2", "t2", "2024-01-01T01:00:00Z", 40),
        ];
        let plan = mk_plan(entries);
        let cfg = BitByBitConfig {
            gap_mode: GapMode::Exact,
            active_slots_cap: 0,
            ..Default::default()
        };
        let got = estimate(&plan, &HashMap::new(), &cfg, None, NOW);
        assert_eq!(got.per_title.len(), 2, "cap=0 leaves every title in scope");
    }

    #[test]
    fn tiered_with_user_unlocks_marks_already_done() {
        // User already has "a". Tiered must still mark it
        // already_done and schedule "b" relative to the tiered
        // anchor. Regression: before the dispatch fix, the
        // classic path took `user_last` from the cache and
        // anchored on it (wrong for shadow semantics), producing
        // schedules that didn't match the live run.
        let entries = vec![
            mk_entry("a", "t1", "2024-01-01T00:00:00Z", 10),
            mk_entry("b", "t1", "2024-01-01T10:00:00Z", 20),
        ];
        let plan = mk_plan(entries);
        let mut my_unlocks: MyUnlocksIndex = HashMap::new();
        let mut set = HashSet::new();
        set.insert("a".to_string());
        my_unlocks.insert("t1".into(), (set, Some(NOW - 3600)));
        let cfg = BitByBitConfig {
            // Concurrent path: this test is ABOUT the gap schedule (sequential mode replaces it with playtime floors).
            sequential: false,
            gap_mode: GapMode::Tiered(vec![(u64::MAX, 1000.0)]),
            ..Default::default()
        };
        let got = estimate(
            &plan,
            &my_unlocks,
            &cfg,
            Some(Some(Duration::from_secs(3600))),
            NOW,
        );
        assert_eq!(got.per_title.len(), 1);
        assert_eq!(got.per_title[0].already_done, 1, "'a' marked already_done");
        // 10h / 1000 = 36s — 'b' easily fits in 1h budget.
        assert_eq!(got.per_title[0].eligible_count, 1, "'b' fits in budget");
    }
}
