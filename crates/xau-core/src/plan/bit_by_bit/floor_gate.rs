//! Sequential-mode playtime-floor precision gate: hold the final achievement until our real MinutesPlayed reaches the target's floor.

use super::*;

/// Sequential-mode PRECISION gate. Before a game's FINAL achievement
/// (the one that flips it to 100%) is fired, verify OUR OWN real Xbox
/// `MinutesPlayed` for the game has actually reached the floor — so we
/// are provably never faster to 100% than the fastest TA speedrunner,
/// measured on the true platform stat, not just wall-clock pacing.
///
/// The wall-clock floor pacing already spread the earlier unlocks over
/// ~`floor_secs` (holding presence the whole time), so by the time we
/// get here our MinutesPlayed is usually already at/above the floor and
/// this returns immediately. It only *waits* to absorb Xbox's stat-
/// propagation lag, re-sending the presence heartbeat each poll so
/// playtime keeps accruing while we wait.
///
/// Returns `Err(())` on cancel (Stop). Two best-effort bail-outs keep
/// a title whose stat never credits presence from hanging the run
/// (both logged, so the gap is auditable): the lag-absorption cap
/// (`FLOOR_GATE_MAX_EXTRA_WAIT_SECS`), and a faster futility exit when
/// the stat has NEVER credited anything after a floor-aware minimum
/// wait plus several consecutive zero reads (`GiveUpNoCredit`).
/// How the playtime-floor gate ended (for the audit trail + report).
/// `Err(())` from [`await_playtime_floor`] means cancelled (Stop); this
/// is the non-cancel outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FloorGateOutcome {
    /// Our real MinutesPlayed reached the floor — the "never faster than
    /// the fastest speedrunner" guarantee held on the true stat.
    Reached,
    /// Fired best-effort without confirming the floor on the stat.
    /// `reason` is a stable slug (`lag_cap` / `no_credit` / `api_outage`)
    /// for the run log; `played_secs` is the last credited playtime seen.
    GaveUp {
        reason: &'static str,
        played_secs: u64,
    },
}

pub(super) async fn await_playtime_floor(
    client: &XboxClient,
    my_xuid: &str,
    title_id: &str,
    floor_secs: u64,
    shared: &Arc<RwLock<BitByBitProgress>>,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    // The run's LIVE presence set. The keepalive heartbeat below must
    // assert every active title, not just ours: Xbox presence is
    // last-write-wins (docs/ARCHITECTURE.md Rule 9), so with a
    // sequential window > 1 a gated title heartbeating only itself
    // would drop its siblings' presence — starving the very
    // MinutesPlayed accrual their own gates are waiting on.
    active_title_ids: &Arc<std::sync::Mutex<Vec<String>>>,
) -> std::result::Result<FloorGateOutcome, ()> {
    let started = Instant::now();
    // Stats are polled for OUR title only; presence asserts the whole set.
    let title_vec = [title_id.to_string()];
    let presence_set = || -> Vec<String> {
        let live = active_title_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if live.is_empty() {
            vec![title_id.to_string()]
        } else {
            live
        }
    };
    // CONSECUTIVE polls where the stats API answered OK but credited
    // ZERO seconds — reset the moment any credit is observed, so
    // scattered degraded reads can't accumulate to a false verdict
    // (adversarial-review catch: the first cut never reset, making the
    // counter cumulative and GiveUpNoCredit fireable on a title whose
    // stat WAS crediting). A long-zero streak on a title that never
    // credited anything means presence never credits playtime here
    // (common for F2P/online games) — waiting the full lag cap is pure
    // dead time, repeated at EVERY such title's final achievement.
    let mut consecutive_zero_polls: u32 = 0;
    // Consecutive polls where the stats endpoint ERRORED (couldn't read
    // our playtime at all). A long run of these means the floor is
    // unverifiable no matter how long we wait — the real cause of the
    // 30-min-per-dead-title stalls in the fidelity audit.
    let mut consecutive_none_polls: u32 = 0;
    // Publish gate telemetry so the GUI can say "⏳ waiting on the
    // playtime floor" instead of guessing. Shipped-bug receipt: in
    // 2.10.3 the rate cell showed "rate-limit window settling — first
    // POSTs incoming" while the runner was actually parked HERE, and
    // the user reported the run as frozen.
    //
    // CONCURRENCY: with a sequential window > 1, several games can be
    // parked here at once. `floor_gate_active` is therefore derived
    // from a COUNTER (`floor_gate_waiting`) rather than assigned — a
    // plain `= false` on the first gate to exit would clear the badge
    // while its siblings are still waiting, which is exactly the
    // 'travou' misreport this telemetry exists to prevent. The
    // waited/played figures stay last-writer-wins: they describe "a
    // gate" and at the default window of 1 there is only ever one.
    let enter = || {
        {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.floor_gate_waiting = w.floor_gate_waiting.saturating_add(1);
            w.floor_gate_active = true;
            w.floor_gate_waited_secs = 0;
            w.floor_gate_played_secs = 0;
        }
        // Flag THIS game's card, so with several games in flight the UI
        // can show which one is waiting instead of one run-wide badge.
        set_active_live_gate(shared, title_id, true, 0);
    };
    // `leave` is called exactly once per `enter`, on every exit path.
    let leave = || {
        {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.floor_gate_waiting = w.floor_gate_waiting.saturating_sub(1);
            w.floor_gate_active = w.floor_gate_waiting > 0;
            if !w.floor_gate_active {
                w.floor_gate_waited_secs = 0;
                w.floor_gate_played_secs = 0;
            }
        }
        set_active_live_gate(shared, title_id, false, 0);
    };
    // Progress ticks while still parked — never touches the counter.
    let publish = |waited: u64, played: u64| {
        {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.floor_gate_waited_secs = waited;
            w.floor_gate_played_secs = played;
        }
        // Feed OUR credited playtime to this game's card so it can show
        // real progress toward the floor, not just a spinner.
        set_active_live_gate(shared, title_id, true, played);
    };
    enter();
    // Last authoritative playtime read — kept across API errors so a
    // transient failure doesn't bounce the GUI's "played Xs" back to 0.
    let mut last_played: u64 = 0;
    loop {
        if *cancel_rx.borrow() {
            leave();
            return Err(());
        }
        // `None` = stats API errored (unknown); `Some(secs)` = the
        // service answered authoritatively (missing map entry = it has
        // no playtime on record for us = a real 0, not an error).
        let fetched: Option<u64> = client
            .get_minutes_played_map(my_xuid, &title_vec)
            .await
            .ok()
            .map(|m| {
                m.get(title_id)
                    .copied()
                    .and_then(|mins| u64::try_from(mins).ok())
                    .unwrap_or(0)
                    .saturating_mul(60)
            });
        match fetched {
            Some(0) => {
                consecutive_zero_polls = consecutive_zero_polls.saturating_add(1);
                consecutive_none_polls = 0;
            }
            // Any observed credit breaks the zero streak — a later
            // Some(0) is then a stale/degraded read (MinutesPlayed is
            // monotonic), never futility evidence.
            Some(_) => {
                consecutive_zero_polls = 0;
                consecutive_none_polls = 0;
            }
            // API error: unknown for the floor, but a long unbroken run
            // of them means the endpoint is unreadable (its own exit).
            None => consecutive_none_polls = consecutive_none_polls.saturating_add(1),
        }
        if let Some(secs) = fetched {
            // Keep the MAX seen: MinutesPlayed is monotonic, so a
            // lower read is stale — and `ever_credited` below must
            // stay true once any credit was observed.
            last_played = last_played.max(secs);
        }
        let waited = started.elapsed().as_secs();
        publish(waited, last_played);
        match floor_gate_decide(
            fetched,
            floor_secs,
            waited,
            consecutive_zero_polls,
            consecutive_none_polls,
            last_played > 0,
        ) {
            FloorGateDecision::Release => {
                tracing::info!(
                    title_id = %title_id,
                    our_played_secs = last_played,
                    floor_secs,
                    "playtime floor reached — clearing the final achievement"
                );
                leave();
                return Ok(FloorGateOutcome::Reached);
            }
            FloorGateDecision::GiveUpLagCap => {
                tracing::warn!(
                    title_id = %title_id,
                    our_played_secs = last_played,
                    floor_secs,
                    "playtime floor gate: our MinutesPlayed did not reach the floor within \
                     the wait cap; firing the final achievement best-effort"
                );
                leave();
                return Ok(FloorGateOutcome::GaveUp {
                    reason: "lag_cap",
                    played_secs: last_played,
                });
            }
            FloorGateDecision::GiveUpNoCredit => {
                tracing::warn!(
                    title_id = %title_id,
                    our_played_secs = last_played,
                    floor_secs,
                    consecutive_zero_polls,
                    "playtime floor gate: stats API answered ZERO credited seconds on every \
                     poll — presence does not credit playtime for this title; firing the \
                     final achievement best-effort instead of burning the full lag cap"
                );
                leave();
                return Ok(FloorGateOutcome::GaveUp {
                    reason: "no_credit",
                    played_secs: last_played,
                });
            }
            FloorGateDecision::GiveUpApiOutage => {
                tracing::warn!(
                    title_id = %title_id,
                    floor_secs,
                    consecutive_none_polls,
                    "playtime floor gate: stats endpoint unreadable on every poll — cannot \
                     verify the floor; firing the final achievement best-effort (wall-clock \
                     pacing already held ~floor)"
                );
                leave();
                return Ok(FloorGateOutcome::GaveUp {
                    reason: "api_outage",
                    played_secs: last_played,
                });
            }
            FloorGateDecision::Wait => {}
        }
        // Keep presence alive so playtime keeps accruing while we wait
        // — for every live title, not just ours (see `presence_set`).
        let _ = client.send_heartbeat_bulk(my_xuid, &presence_set()).await;
        tokio::select! {
            biased;
            r = cancel_rx.changed() => {
                if r.is_err() || *cancel_rx.borrow() {
                    leave();
                    return Err(());
                }
            }
            () = tokio::time::sleep(Duration::from_secs(FLOOR_GATE_POLL_SECS)) => {}
        }
    }
}

/// One poll's verdict for the playtime-floor gate. Pure so the
/// boundary conditions are unit-testable without an HTTP client.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FloorGateDecision {
    /// Our credited playtime reached the floor — fire the final
    /// achievement with the "never faster than the fastest TA
    /// speedrunner" guarantee intact.
    Release,
    /// Keep polling (stat lagging but plausible, or API hiccup).
    Wait,
    /// Waited the (floor-proportional) lag-absorption cap — fire
    /// best-effort.
    GiveUpLagCap,
    /// The stats service answered ZERO on every poll and NO credit was
    /// ever observed: presence never credits playtime here; more
    /// waiting can't help the guarantee, so fire best-effort early.
    GiveUpNoCredit,
    /// The stats endpoint ERRORED on every poll for a long run — we
    /// cannot read our playtime at all, so we cannot verify the floor
    /// no matter how long we wait. Fire best-effort (wall-clock floor
    /// pacing already held ~floor). Kills the 30-min-per-dead-title
    /// stall the replica-fidelity audit found (5 code:49 titles each
    /// burned the full cap because every stats read errored).
    GiveUpApiOutage,
}

/// Gate poll cadence. 45 s is gentle on the stats endpoint while
/// bounding release latency to under a minute past the floor.
pub(super) const FLOOR_GATE_POLL_SECS: u64 = 45;

/// Absolute ceiling on the EXTRA wait on top of the ~floor already
/// elapsed during pacing — this is only lag absorption, not the whole
/// playtime. The EFFECTIVE cap is floor-proportional (see
/// [`floor_gate_lag_cap_secs`]) so a 60 s-floor title can never park
/// the full 30 min.
pub(super) const FLOOR_GATE_MAX_EXTRA_WAIT_SECS: u64 = 1800;

/// Consecutive authoritative-zero polls required before the futility
/// exit can even be considered (~3 min of back-to-back zeros at the
/// 45 s cadence — the first poll fires immediately on gate entry).
pub(super) const FLOOR_GATE_MAX_ZERO_POLLS: u32 = 5;

/// Consecutive stats-API-error polls before concluding the endpoint is
/// unreadable and the floor can't be verified (~4.5 min at 45 s).
pub(super) const FLOOR_GATE_MAX_NONE_POLLS: u32 = 6;

/// Floor-proportional lag cap: `min(1800, max(2*floor, 300))`. A tiny
/// 60 s floor caps at 5 min (not 30); a multi-hour floor keeps the full
/// 30 min. Bounds dead-title stalls to the floor's own scale.
pub(super) fn floor_gate_lag_cap_secs(floor_secs: u64) -> u64 {
    floor_secs
        .saturating_mul(2)
        .max(300)
        .min(FLOOR_GATE_MAX_EXTRA_WAIT_SECS)
}

/// Minimum time the gate must have waited before `GiveUpNoCredit` may
/// fire, scaled to the floor: `max(300, min(floor, 600))`. Small
/// floors (the 60-120 s shovelware norm) get >=5 min for the FIRST
/// minute of credit to propagate (MinutesPlayed is minute-granular and
/// lags); large floors allow up to 10 min. Without this, the futility
/// exit fires at ~3 min — before a normally-crediting title's first
/// minute can even show up — and would become the DEFAULT exit for
/// small floors (adversarial-review calibration).
pub(super) fn floor_gate_no_credit_min_wait_secs(floor_secs: u64) -> u64 {
    floor_secs.min(600).max(300)
}

/// Record how a title's playtime-floor gate ended: emit a
/// `bbb_floor_gate` line into the run log (auditable provenance for
/// spec item 4) and, on a best-effort give-up, bump the snapshot
/// counter + set the trouble string so the GUI never hides it.
pub(super) fn record_floor_gate_outcome(
    shared: &Arc<RwLock<BitByBitProgress>>,
    run_id: &str,
    title_id: &str,
    game: &str,
    floor_secs: u64,
    outcome: &FloorGateOutcome,
) {
    let (decision, played) = match outcome {
        FloorGateOutcome::Reached => ("reached", floor_secs),
        FloorGateOutcome::GaveUp {
            reason,
            played_secs,
        } => (*reason, *played_secs),
    };
    let line = serde_json::json!({
        "kind": "bbb_floor_gate",
        "run_id": run_id,
        "title_id": title_id,
        "game": game,
        "floor_secs": floor_secs,
        "played_secs": played,
        "decision": decision,
        "ts_ms": wallclock_ms(),
    })
    .to_string();
    let _ = crate::plan::runner::append_bbb_run_line(run_id, &line);
    if let FloorGateOutcome::GaveUp { .. } = outcome {
        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.floor_gate_giveups = w.floor_gate_giveups.saturating_add(1);
        w.last_floor_gate_giveup = Some(format!("{game} — floor unconfirmed ({decision})"));
    }
}

pub(super) fn floor_gate_decide(
    fetched: Option<u64>,
    floor_secs: u64,
    waited_secs: u64,
    consecutive_zero_polls: u32,
    consecutive_none_polls: u32,
    ever_credited: bool,
) -> FloorGateDecision {
    // A satisfied floor wins over every give-up condition.
    if let Some(secs) = fetched {
        if secs >= floor_secs {
            return FloorGateDecision::Release;
        }
    }
    if waited_secs >= floor_gate_lag_cap_secs(floor_secs) {
        return FloorGateDecision::GiveUpLagCap;
    }
    // Persistent stats-API outage: we can't READ our playtime, so more
    // waiting cannot verify the floor. Requires the floor-aware minimum
    // wait too, so a brief hiccup doesn't trip it.
    if consecutive_none_polls >= FLOOR_GATE_MAX_NONE_POLLS
        && waited_secs >= floor_gate_no_credit_min_wait_secs(floor_secs)
    {
        return FloorGateDecision::GiveUpApiOutage;
    }
    // Futility requires ALL of: the stat NEVER credited anything (one
    // observed credit proves the title credits — then only the lag cap
    // may end the wait), a long unbroken zero streak, and a floor-aware
    // minimum wait so minute-granularity + propagation lag can't be
    // mistaken for "never credits".
    if !ever_credited
        && consecutive_zero_polls >= FLOOR_GATE_MAX_ZERO_POLLS
        && waited_secs >= floor_gate_no_credit_min_wait_secs(floor_secs)
    {
        return FloorGateDecision::GiveUpNoCredit;
    }
    FloorGateDecision::Wait
}
