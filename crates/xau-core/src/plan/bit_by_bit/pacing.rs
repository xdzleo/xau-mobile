//! Wall-clock pacing + rate limiting: floor-adjusted per-title targets, the proactive unlock pacer, and the cancellable sleep-to-target.

use super::*;

/// Xbox's achievements-endpoint unlock budget: 100 requests per 15
/// seconds, sustained. Kept as the two integer facts (not the derived
/// 6.67 float) so there's ONE source of truth shared by the runner's
/// pacer and the forecast's throughput cap.
pub const UNLOCK_RATE_LIMIT_REQUESTS: u64 = 100;

/// Window for [`UNLOCK_RATE_LIMIT_REQUESTS`], in milliseconds.
pub const UNLOCK_RATE_LIMIT_PERIOD_MS: u64 = 15_000;

/// Minimum spacing between consecutive unlock POSTs (150 ms) so the
/// combined rate across all concurrent title workers stays under the
/// budget above.
pub(super) const UNLOCK_MIN_INTERVAL: Duration =
    Duration::from_millis(UNLOCK_RATE_LIMIT_PERIOD_MS / UNLOCK_RATE_LIMIT_REQUESTS);

/// Proactive token-bucket pacer for unlock POSTs, shared across every
/// title worker in a run.
///
/// The per-POST semaphore bounds CONCURRENCY (≤8 in flight); it does
/// NOT bound the request RATE, so 8 fast titles would otherwise burst
/// 40-80 req/s — far over Xbox's 100-req/15s budget — until a 429
/// throttled them reactively (an overshoot→park→overshoot sawtooth that
/// can exhaust a POST's retries and surface a FAILED unlock).
///
/// This reserves the next send slot, spaced ≥ [`UNLOCK_MIN_INTERVAL`]
/// after the previous reservation, so the aggregate POST rate can never
/// exceed the budget — **no 429, no rate-limit failure, by
/// construction**. The reactive throttle gate in `xbox::client::retry`
/// stays as a backstop. Reservation is O(1) under the mutex; the wait
/// happens outside the lock. Max wait for any one call ≈ concurrency ×
/// interval (~1.2 s at 8), so Stop latency stays small.
pub(super) async fn pace_unlock(gate: &tokio::sync::Mutex<Instant>) {
    let send_at = {
        let mut next = gate.lock().await;
        // Slot = later of (now, previously-reserved). Reserve the one
        // after it for the next caller.
        let slot = (*next).max(Instant::now());
        *next = slot + UNLOCK_MIN_INTERVAL;
        slot
    };
    let wait = send_at.saturating_duration_since(Instant::now());
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

/// Sequential-mode per-game pacing: given the game went "online"
/// (heartbeat) at `title_start_ms` and has `n` unlocks to fire, return
/// the wall-clock `(unix_secs, subsec_ms)` at which unlock `idx`
/// (0-based) should fire so the whole game is spread EVENLY across
/// `floor_secs` — entry `idx` lands at `start + (idx+1)/n * floor`, so
/// the last (idx = n-1) lands at exactly `start + floor` and the game
/// can never reach 100% faster than the floor.
///
/// `floor_secs == 0` (concurrent mode) or `n == 0` → returns the
/// caller's original plan-scheduled target unchanged, so the default
/// path keeps honoring the gap_mode schedule verbatim.
///
/// Monotonic in `idx` (so wire order == chronology is preserved) and
/// fully saturating (no overflow/panic on adversarial spans). Feeding
/// the result to `sleep_until_scheduled_cancellable` keeps the cancel
/// race + long-wait presence refresh for free.
pub(super) fn floor_adjusted_target(
    plan: Option<(i64, u16)>,
    floor_secs: u64,
    title_start_ms: i64,
    idx: usize,
    n: usize,
) -> Option<(i64, u16)> {
    if floor_secs == 0 || n == 0 {
        return plan;
    }
    let floor_ms = i64::try_from(floor_secs)
        .unwrap_or(i64::MAX / 2)
        .saturating_mul(1000);
    let n_i = i64::try_from(n).unwrap_or(i64::MAX);
    let step = i64::try_from(idx).unwrap_or(i64::MAX).saturating_add(1);
    // (idx+1) * floor / n — integer, monotonic non-decreasing in idx.
    let offset_ms = floor_ms.saturating_mul(step) / n_i.max(1);
    let target_ms = title_start_ms.saturating_add(offset_ms);
    let secs = target_ms.div_euclid(1000);
    let subsec = u16::try_from(target_ms.rem_euclid(1000)).unwrap_or(0);
    Some((secs, subsec))
}

/// Sequential-mode pacing that REPLAYS the target speedrunner's real
/// intra-title cadence instead of an even metronome.
///
/// The fidelity audit's #1 finding: even-spread (`floor_adjusted_target`)
/// places unlocks at exactly `floor/n` apart — 0 % variance, the single
/// clearest bot tell on a public profile. Real 100 % runs are bursty
/// (several cheevos on one boss, long grind gaps, endgame clusters).
///
/// This maps each entry's RELATIVE offset from the LAST plan unlock onto
/// the floor window, anchored at the end:
///   `target_i = start + floor - (sched_last - sched_i) * min(1, floor/span)`
/// so the last entry still lands at exactly `start + floor` (the
/// never-faster invariant + floor gate are untouched), the shape of the
/// speedrunner's gaps is preserved verbatim when their unlock span fits
/// inside the floor (the normal case, since MinutesPlayed >= span), and
/// is uniformly compressed otherwise. Monotonic in `idx` because the
/// affine map on a non-decreasing `sched` chain is non-decreasing.
///
/// Falls back to even-spread ([`floor_adjusted_target`] semantics) when
/// the cadence can't be mapped: `floor_secs == 0`, fewer than 2 entries,
/// a zero/degenerate span (same-second burst — replaying it fast IS
/// faithful), or any missing plan time. Returns one target per input
/// entry, in order.
pub(super) fn floor_adjusted_targets(
    scheduled: &[Option<(i64, u16)>],
    floor_secs: u64,
    title_start_ms: i64,
) -> Vec<Option<(i64, u16)>> {
    let n = scheduled.len();
    let even = || {
        (0..n)
            .map(|idx| floor_adjusted_target(scheduled[idx], floor_secs, title_start_ms, idx, n))
            .collect::<Vec<_>>()
    };
    if floor_secs == 0 || n < 2 {
        return even();
    }
    // Every entry must carry a plan time to map relative gaps.
    let sched_ms: Vec<i64> = match scheduled
        .iter()
        .map(|o| o.map(|(s, ms)| s.saturating_mul(1000).saturating_add(i64::from(ms))))
        .collect::<Option<Vec<i64>>>()
    {
        Some(v) => v,
        None => return even(),
    };
    let first = sched_ms[0];
    let last = sched_ms[n - 1];
    let span = last.saturating_sub(first);
    if span <= 0 {
        // Same-second burst (or non-monotonic) — even-spread is the
        // safe, faithful fallback.
        return even();
    }
    let floor_ms = i64::try_from(floor_secs)
        .unwrap_or(i64::MAX / 2)
        .saturating_mul(1000);
    let anchor = title_start_ms.saturating_add(floor_ms);
    // scale = min(1, floor/span) as a rational, applied as
    // offset_from_end * floor / max(span, floor) to stay in integer math
    // (denominator = max(span,floor) makes the factor <= 1).
    let denom = span.max(floor_ms).max(1);
    let mut out: Vec<Option<(i64, u16)>> = Vec::with_capacity(n);
    let mut prev_ms = i64::MIN;
    for &s in &sched_ms {
        let offset_from_end = last.saturating_sub(s); // >= 0, non-increasing in idx
        let scaled =
            (i128::from(offset_from_end) * i128::from(floor_ms) / i128::from(denom)) as i64;
        let mut target_ms = anchor.saturating_sub(scaled);
        // Guard strict monotonicity against any integer-rounding tie.
        if target_ms < prev_ms {
            target_ms = prev_ms;
        }
        prev_ms = target_ms;
        let secs = target_ms.div_euclid(1000);
        let subsec = u16::try_from(target_ms.rem_euclid(1000)).unwrap_or(0);
        out.push(Some((secs, subsec)));
    }
    out
}

/// Sleep until the target's scheduled unlock time before posting.
///
/// Called from the per-id dispatch loops in `process_title`. The
/// purpose is forensic-perfect timing: a third party inspecting
/// the achievement's server-side `timeUnlocked` stamps should see
/// the same cadence the TARGET produced, not a burst of unlocks
/// all sharing one server timestamp.
///
/// Three cases:
/// - `scheduled` is `None` or `<= now` → no-op, post immediately.
///   This is the common case with `time_budget = None`: the
///   scheduler filter only admits entries whose `scheduled_unix
///   <= eligibility_horizon == now`, so by the time they reach
///   dispatch they're all "fire now".
/// - `scheduled - now <= 30 s` → sleep the delta. Short waits
///   are cheap and don't need presence refresh because the
///   per-title heartbeat that fired at the start of
///   `process_title` is still well within Xbox's 5-min TTL.
/// - `scheduled - now > 30 s` → sleep the delta AND re-send the
///   per-title presence heartbeat right before posting, so Xbox's
///   record shows the user "present in this title" at the exact
///   unlock moment. Without that refresh, a several-minute sleep
///   could let presence TTL expire and the subsequent unlock
///   would (a) look forensically weird (unlocked while not
///   present) and (b) fail on event-based titles.
///
/// Best-effort on the heartbeat: a transient failure is logged at
/// debug level and the unlock proceeds — the unlock itself would
/// surface a real error if presence is truly mandatory.
/// Cancellable variant of [`sleep_until_scheduled`] that races the
/// sleep against a `watch::Receiver<bool>` cancel signal. Returns
/// `Err(())` when the sleep was interrupted by cancellation (caller
/// should stop its loop), `Ok(())` when the sleep completed naturally
/// (or was a no-op because `scheduled <= now`).
///
/// Why this exists: the previous `sleep_until_scheduled` was a plain
/// `tokio::time::sleep` that could run for minutes/hours when the
/// target's plan has future-scheduled entries inside the user's
/// `time_budget`. While that sleep was in flight, the only stop
/// signal check was in the outer drain loop's `join_next` boundary
/// — meaning Stop clicks took as long as the remaining sleep to
/// honour. This helper makes Stop effective in <1 ms. See audit bug
/// BBB-Sleep-Cancel#1.
pub(super) async fn sleep_until_scheduled_cancellable(
    // `(scheduled_unix_secs, scheduled_subsec_ms)` — combined into a
    // single ms target via `secs * 1000 + subsec_ms`. Splitting it
    // into a pair instead of pre-multiplying lets the caller use the
    // same secs value for the eligibility filter (which works in
    // seconds) without extra back-and-forth conversions. See
    // `BBB-Subsec` on `ScheduledEntry` for why sub-second matters
    // here: same-second source bursts fire at their original
    // cadence (`.100`, `.500`, `.999`) instead of back-to-back at
    // network speed.
    scheduled: Option<(i64, u16)>,
    client: &XboxClient,
    my_xuid: &str,
    title_id: &str,
    // The run's LIVE presence set at call time. Xbox presence is
    // last-write-wins (docs/ARCHITECTURE.md Rule 9): a heartbeat
    // ASSERTS the full set of titles you are present in. This is the
    // most frequent heartbeat on the sequential path — it fires after
    // every paced sleep longer than `PRESENCE_REFRESH_THRESHOLD_MS`,
    // which floor pacing makes nearly every sleep — so asserting only
    // `title_id` here would repeatedly drop every OTHER in-flight
    // game's presence, starving the MinutesPlayed their own floor
    // gates poll. Empty slice = fall back to `title_id` alone.
    presence_titles: &[String],
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> std::result::Result<(), ()> {
    // Already cancelled? Honor it immediately.
    if *cancel_rx.borrow() {
        return Err(());
    }
    let Some((target_secs, subsec_ms)) = scheduled else {
        return Ok(());
    };
    let target_ms = target_secs
        .saturating_mul(1000)
        .saturating_add(i64::from(subsec_ms));
    let now_initial_ms = now_unix_ms();
    let initial_wait_ms_i64 = target_ms.saturating_sub(now_initial_ms);
    if initial_wait_ms_i64 <= 0 {
        return Ok(());
    }
    let initial_wait_ms = u64::try_from(initial_wait_ms_i64).unwrap_or(u64::MAX);
    // Threshold for "this wait is long enough to bother refreshing
    // presence afterwards" — kept at 30 s in ms.
    const PRESENCE_REFRESH_THRESHOLD_MS: u64 = 30_000;
    if initial_wait_ms > PRESENCE_REFRESH_THRESHOLD_MS {
        tracing::debug!(
            wait_ms = initial_wait_ms,
            title_id = %title_id,
            "sleep_until_scheduled_cancellable: long wait — will refresh presence after sleep"
        );
    }
    // BBB-Timing-3 — wall-clock fence. Cap each individual
    // `tokio::time::sleep` at this many milliseconds, then
    // re-evaluate `target_ms - now_unix_ms()` against the OS
    // wall-clock and decide whether to fire or keep sleeping.
    //
    // **Why this isn't paranoia:** `tokio::time::sleep` runs on a
    // monotonic clock derived from the runtime's start instant. On
    // OS suspend (laptop closed) the monotonic clock typically
    // pauses or jumps with the wall-clock; the behaviour is
    // platform-specific and not contractual. NTP step corrections
    // and DST transitions also introduce wall-clock drift the
    // monotonic timer is blind to. Without a fence, a multi-day
    // sleep against `GapMode::Exact` could fire seconds-to-hours
    // off the intended `target` — exactly the cadence-fidelity
    // hole the bit-by-bit replay must avoid.
    //
    // 5 minutes (300_000 ms) is a sweet spot: short enough that a
    // wall-clock step gets absorbed within the user's tolerance
    // window, long enough that the cost (~12 wakeups/hour for an
    // idle title) is trivial compared to the unlock POST itself.
    // See audit BBB-Timing-3 / Q6.
    const FENCE_CHUNK_MS: u64 = 300_000;
    //
    // Race sleep vs cancel. `biased;` ensures cancel is polled first
    // each loop-iteration so an already-set cancel signal wins
    // deterministically over a just-elapsed sleep.
    //
    // BBB-7: a `cancel_rx.changed()` returning `Err(_)` means the
    // sender was dropped — the run state is being torn down. Treat
    // that as cancel rather than falling through to a NON-cancellable
    // fallback sleep. The previous code held a `tokio::time::sleep`
    // inside the cancel arm with no select, so any further Stop click
    // had to wait the full residual.
    'fence: loop {
        let now_ms = now_unix_ms();
        let remaining_ms_i64 = target_ms.saturating_sub(now_ms);
        if remaining_ms_i64 <= 0 {
            // Target already met (or passed) per wall-clock — stop
            // sleeping even if the monotonic timer says otherwise.
            break;
        }
        let chunk_ms = u64::try_from(remaining_ms_i64)
            .unwrap_or(u64::MAX)
            .min(FENCE_CHUNK_MS);
        let mut sleeper = std::pin::pin!(tokio::time::sleep(Duration::from_millis(chunk_ms)));
        loop {
            tokio::select! {
                biased;
                res = cancel_rx.changed() => {
                    if res.is_err() {
                        return Err(());
                    }
                    if *cancel_rx.borrow() {
                        return Err(());
                    }
                    continue; // spurious flip — keep waiting in this chunk
                }
                () = &mut sleeper => continue 'fence,
            }
        }
    }
    if initial_wait_ms > PRESENCE_REFRESH_THRESHOLD_MS {
        // Refresh presence after long wait so the impending unlock
        // lands with a fresh presence record.
        //
        // BBB-3: the heartbeat POST is itself raced against cancel so
        // a Stop click here does not have to wait the full HTTP RTT
        // before being honoured. On cancel we drop the in-flight
        // heartbeat; presence-refresh is best-effort already.
        let fallback = [title_id.to_string()];
        let titles: &[String] = if presence_titles.is_empty() {
            &fallback
        } else {
            presence_titles
        };
        let heartbeat = client.send_heartbeat_bulk(my_xuid, titles);
        tokio::select! {
            biased;
            res = cancel_rx.changed() => {
                if res.is_err() || *cancel_rx.borrow() {
                    return Err(());
                }
            }
            res = heartbeat => {
                if let Err(e) = res {
                    tracing::debug!(
                        ?e,
                        title_id = %title_id,
                        "presence refresh after long sleep failed; unlock will proceed anyway"
                    );
                }
            }
        }
    }
    Ok(())
}

// BUG HUNTER M-dead-code-removed: `sleep_until_scheduled` (the
// non-cancellable variant) was deleted — every production caller
// uses the `_cancellable` variant introduced by BBB-2 to fix the
// "Stop click during long sleep doesn't propagate" bug. Leaving a
// non-cancellable wall-clock sleep in the public API was a foot-gun
// that the audit explicitly removed.
