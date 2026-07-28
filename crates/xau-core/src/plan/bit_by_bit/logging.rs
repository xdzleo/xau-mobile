//! Forensic run-log writers: `bbb_run_start`/`bbb_run_end` headers and the per-unlock `bbb_wire` line.

use super::*;

/// Append the `bbb_run_start` header (config echo + plan info) to the
/// shared history AND this run's dedicated log file.
pub(super) fn write_bbb_run_start(
    run_id: &str,
    my_xuid: &str,
    plan_source: &str,
    plan_total: usize,
    config: &BitByBitConfig,
) {
    let gap = match &config.gap_mode {
        GapMode::Exact => "exact".to_string(),
        GapMode::Capped(d) => format!("capped:{}s", d.as_secs()),
        GapMode::Scaled(f) => format!("scaled:{f}"),
        GapMode::Tiered(_) => "tiered".to_string(),
    };
    let line = serde_json::json!({
        "kind": "bbb_run_start",
        "run_id": run_id,
        "xuid": my_xuid,
        "ts_ms": now_unix_ms(),
        "plan_source": plan_source,
        "plan_total": plan_total,
        "config": {
            "gap_mode": gap,
            "max_speedup": config.max_speedup.as_str(),
            "catch_up": format!("{:?}", config.catch_up),
            "time_budget_secs": config.time_budget.map(|d| d.as_secs()),
            "top_n_titles": config.top_n_titles,
            "active_slots_cap": config.active_slots_cap,
            "title_concurrency": config.title_concurrency,
            "max_entries": config.max_entries,
            "max_gamerscore": config.max_gamerscore,
            // Dispatch mode — without these the self-contained run log
            // cannot prove it was sequential (it echoes concurrency:6),
            // and an auditor can only infer the mode from POST spacing.
            "sequential": config.sequential,
            "min_game_seconds": config.min_game_seconds,
        },
    })
    .to_string();
    let _ = crate::plan::runner::append_history_line(&line);
    let _ = crate::plan::runner::append_bbb_run_line(run_id, &line);
}

/// Append the `bbb_run_end` summary (final counts) to the shared
/// history AND this run's dedicated log file.
pub(super) fn write_bbb_run_end(run_id: &str, snap: &BitByBitProgress) {
    let line = serde_json::json!({
        "kind": "bbb_run_end",
        "run_id": run_id,
        "ts_ms": now_unix_ms(),
        "state": format!("{:?}", snap.state),
        "unlocked": snap.unlocked_this_run,
        "gamerscore": snap.gamerscore_this_run,
        "already_done": snap.already_done,
        "scheduled_for_future": snap.scheduled_for_future,
        "event_based_skipped_titles": snap.event_based_skipped_titles,
        "unauthorized_skipped_titles": snap.unauthorized_skipped_titles,
        "token_expired": snap.token_expired,
        "errored_titles": snap.errored_titles,
        "last_error": snap.last_error,
    })
    .to_string();
    let _ = crate::plan::runner::append_history_line(&line);
    let _ = crate::plan::runner::append_bbb_run_line(run_id, &line);
}

/// Append one JSONL line to `unlock-history.jsonl` documenting the
/// wire-level outcome of a single bit-by-bit POST.
///
/// **Why this exists:** the standard `progress.unlocked_this_run`
/// counter says "X out of Y landed" but is forensically opaque —
/// after a 240k-entry run finishes, there is no way to prove the
/// POSTs actually went in the order the plan demanded, or how long
/// each one took on the wire. This log records `(scheduled_unix,
/// ts_sent, ts_resp, status)` per POST attempt so:
///
/// 1. The wire-order can be reconstructed by sorting the JSONL on
///    `ts_sent_ms` and confirming it matches `scheduled_unix` order
///    within each `title_id`.
/// 2. Latency outliers (a POST that took 3 s instead of 80 ms) are
///    individually visible.
/// 3. A post-hoc analyst can compare with Xbox-side `time_unlocked`
///    timestamps in their own profile to detect timestamp drift.
///
/// All fields are best-effort; failure to write the log line is
/// swallowed (it must NEVER be allowed to break the unlock loop).
pub(super) fn log_wire_event(
    title_id: &str,
    achievement_id: &str,
    // `(scheduled_unix_secs, scheduled_subsec_ms)` — the PLAN pair from
    // the source timeline. Logged for provenance.
    scheduled: Option<(i64, u16)>,
    // The EFFECTIVE target the runner actually slept to (floor-adjusted
    // in sequential mode; identical to `scheduled` in concurrent mode).
    // `drift_ms` is measured against THIS, so a perfectly floor-paced
    // sequential run reports ~0 drift instead of the millions-of-ms
    // staircase the fidelity audit found (drift was vs the bypassed
    // plan schedule). `None` falls back to `scheduled`.
    effective: Option<(i64, u16)>,
    ts_sent_ms: i64,
    ts_resp_ms: i64,
    status: &str,
    error: Option<&str>,
    // BUG HUNTER M-bbb-wirelog-xuid: profile attribution. Without
    // this field, an `unlock-history.jsonl` shared across two
    // accounts running on the same machine couldn't be filtered
    // per-profile by an auditor. Defaults to empty string when the
    // caller doesn't have a xuid handy (e.g. unit tests with mock
    // clients) — empty means "unknown" for filter consumers.
    my_xuid: &str,
    // Per-run identifier so events can be grouped/filtered by run in
    // the shared history, and mirrored into this run's dedicated log.
    run_id: &str,
    // Human-readable game name so the raw log + report show the game,
    // not just the numeric title_id.
    game: &str,
) {
    let scheduled_unix = scheduled.map(|(s, _)| s);
    let scheduled_subsec_ms = scheduled.map(|(_, ms)| ms);
    let scheduled_ms =
        scheduled.map(|(s, ms)| s.saturating_mul(1000).saturating_add(i64::from(ms)));
    // The target actually aimed at (floor-adjusted in sequential mode).
    // Drift is measured against this so it stays small + bounded even
    // when the plan schedule was deliberately replaced by floor pacing.
    let effective_ms = effective
        .map(|(s, ms)| s.saturating_mul(1000).saturating_add(i64::from(ms)))
        .or(scheduled_ms);
    let line = serde_json::json!({
        "kind": "bbb_wire",
        "run_id": run_id,
        "xuid": my_xuid,
        "title_id": title_id,
        "game": game,
        "ach_id": achievement_id,
        "scheduled_unix": scheduled_unix,
        "scheduled_subsec_ms": scheduled_subsec_ms,
        "scheduled_ms": scheduled_ms,
        // The floor-adjusted target the runner actually slept to.
        "effective_scheduled_ms": effective_ms,
        "ts_sent_ms": ts_sent_ms,
        "ts_resp_ms": ts_resp_ms,
        "elapsed_ms": ts_resp_ms.saturating_sub(ts_sent_ms),
        // Drift = how late the POST went out vs the EFFECTIVE target
        // (floor-adjusted in sequential mode). Negative means we beat
        // it (rare). This is the fidelity signal: drift should be small
        // + bounded by network + tokio sleep granularity, NOT the
        // millions-of-ms staircase that measuring vs the bypassed plan
        // schedule produced.
        "drift_ms": effective_ms.map(|s| ts_sent_ms.saturating_sub(s)),
        "status": status,
        "error": error,
    })
    .to_string();
    // Shared rolling history (filterable by run_id) + this run's own
    // self-contained log file. Both best-effort.
    let _ = crate::plan::runner::append_history_line(&line);
    let _ = crate::plan::runner::append_bbb_run_line(run_id, &line);
}
