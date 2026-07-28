//! The Xbox presence keepalive loop that heartbeats the active titles so event-based unlocks commit.

use super::*;

/// Abstract sender for the presence keepalive loop. `XboxClient`
/// implements this via its real heartbeat/stop endpoints; tests
/// implement it with a counter so we can verify cadence + cancel
/// semantics without a real HTTP server.
#[async_trait::async_trait]
pub(crate) trait PresenceSender: Send + Sync + 'static {
    /// Send a presence heartbeat covering the given title_ids.
    /// Return value is discarded; errors are logged at debug level
    /// inside the loop so tests can return `Ok(())` unconditionally.
    async fn send(&self, title_ids: &[String]) -> Result<()>;
    /// Release the presence record on cancellation. Best-effort.
    async fn stop(&self);
}

#[async_trait::async_trait]
impl PresenceSender for (XboxClient, String) {
    async fn send(&self, title_ids: &[String]) -> Result<()> {
        self.0.send_heartbeat_bulk(&self.1, title_ids).await?;
        Ok(())
    }
    async fn stop(&self) {
        // DO NOT call `stop_heartbeat` — the Xbox DELETE endpoint at
        // /users/xuid()/devices/current/titles/current is UNSCOPED:
        // it wipes every title presence the xuid currently has,
        // including whatever spoof_farm registered in parallel.
        // Instead, just stop sending heartbeats; Xbox's 5-minute
        // TTL reaps the title entries we stopped refreshing while
        // leaving spoof_farm's live ones alone. See audit bug
        // BBB-Presence#3 + ARCHITECTURE.md Rule 9 (central
        // arbitration for shared /presence endpoint).
        tracing::debug!("bit-by-bit presence loop ended; relying on Xbox 5-min TTL for cleanup");
    }
}

/// Presence keepalive loop. Extracted from `run()` so it can be unit-
/// tested with `tokio::time::pause()` + a mock sender; the extraction
/// also keeps the main runner body linear.
///
/// Contract: fires `sender.send` immediately (first `tick` on a fresh
/// `interval` fires at t=0), then every `period` until `rx` receives.
/// On exit, best-effort `sender.stop` to release the presence record —
/// failure here is silently ignored because Xbox's own TTL (~5 min)
/// guarantees cleanup regardless.
///
/// Failures in the periodic heartbeat are logged at `debug!` only —
/// a transient network hiccup shouldn't abort the whole bit-by-bit
/// run; presence just resumes on the next tick (or expires after TTL
/// and the next tick rebuilds it).
pub(crate) async fn presence_keepalive_loop<S: PresenceSender>(
    sender: S,
    // Shared, DYNAMIC set of the title_ids currently being processed
    // (each `process_title` inserts its id on activation and removes it
    // on completion). The loop heartbeats exactly this live set each
    // tick, so a title that finishes STOPS being refreshed and its
    // Xbox presence lapses via TTL — freeing a slot for the next title
    // to rotate in. Capped at `MAX_SIMULTANEOUS_TITLES` (8) both here
    // and by the activation semaphore, so we never broadcast presence
    // for more than 8 titles at once (Xbox silently drops the extras).
    // Replaces the old fixed `Vec<String>` snapshot taken once at start,
    // which kept dead titles' presence alive for the whole run.
    active_title_ids: Arc<std::sync::Mutex<Vec<String>>>,
    period: Duration,
    mut rx: oneshot::Receiver<()>,
) {
    // Wave-9 audit forensic-HIGH-5: a perfectly-periodic 180s
    // sawtooth in the heartbeat-delta distribution is a
    // fingerprint — real Xbox clients (UWP / console / cloud)
    // jitter their heartbeats. A wireshark/server-log analyst
    // looking at our wire log sees `Δt = 180.00s` with 0%
    // variance, which doesn't match any real client. Randomise
    // each tick interval by ±15% so the per-tick deltas spread
    // across roughly 153s..207s, matching observed real-client
    // distributions. Uses `OsRng` (already a workspace dep via
    // `rand_core`) to avoid pulling a new RNG dep just for this.
    use rand_core::{OsRng, RngCore};
    let base_ms = u64::try_from(period.as_millis()).unwrap_or(180_000);
    // ±15% window: 30% wide, centred on `base_ms`.
    let jitter_window_ms = (base_ms * 30) / 100;
    let jitter_floor_ms = base_ms.saturating_sub(jitter_window_ms / 2);
    let pick_interval = || {
        // u32 modulo is fine — `jitter_window_ms` is at most
        // (base_ms * 30 / 100) which for any plausible period
        // (≤ 1h) fits in u32 with room to spare.
        let span = u32::try_from(jitter_window_ms.max(1)).unwrap_or(u32::MAX);
        let offset = u64::from(OsRng.next_u32() % span);
        Duration::from_millis(jitter_floor_ms.saturating_add(offset))
    };
    // First tick fires immediately (matches the prior
    // `tokio::time::interval(period)` semantics — Xbox expects an
    // initial heartbeat at start-of-session) so the explicit bulk
    // heartbeat sent by the caller is followed promptly by this
    // loop's contribution. Only SUBSEQUENT intervals are jittered.
    let mut first_iteration = true;
    loop {
        let next_interval = if first_iteration {
            first_iteration = false;
            Duration::ZERO
        } else {
            pick_interval()
        };
        tokio::select! {
            // `biased;` so the cancel arm is polled BEFORE the tick
            // arm on each iteration. Without it, tokio's fair
            // selection occasionally picked the tick arm even when
            // rx had already resolved (a coin-flip on which arm
            // ran), producing one extra heartbeat POST AFTER the
            // user asked us to stop.
            biased;
            _ = &mut rx => {
                sender.stop().await;
                return;
            }
            _ = tokio::time::sleep(next_interval) => {
                // Inner biased race: between the tick resolving and
                // `sender.send` reaching the HTTP client's first
                // await point, a Stop click may arrive. Without this
                // inner select, the send proceeded to completion
                // even though cancel had already fired — an orphan
                // POST that burned a rate-limit slot and left an
                // "Xbox recent presence" record that outlived the
                // run. Audit: Agent-2 + Agent-4 cross-validated
                // MEDIUM. Once the HTTP request is committed to the
                // wire (reqwest has no cancellation token), the
                // server will still record it; we just don't await
                // the response. That's an acceptable degradation
                // vs the status quo (orphan POST that blocks the
                // outer loop until it returns).
                // Snapshot the live active set (lock + clone + DROP the
                // guard) BEFORE the awaited send — never hold a
                // std::sync::Mutex across an await. Cap at the Xbox
                // presence ceiling. Empty set (a lull between titles) →
                // skip the POST entirely rather than send an empty body.
                let snapshot: Vec<String> = {
                    let guard = active_title_ids
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.iter().take(MAX_SIMULTANEOUS_TITLES).cloned().collect()
                };
                if snapshot.is_empty() {
                    continue;
                }
                tokio::select! {
                    biased;
                    _ = &mut rx => {
                        sender.stop().await;
                        return;
                    }
                    res = sender.send(&snapshot) => {
                        if let Err(e) = res {
                            tracing::debug!(
                                ?e,
                                "presence keepalive heartbeat failed (unlock path may still work)"
                            );
                        }
                    }
                }
            }
        }
    }
}

// ==========================================================================
//  PURE SCHEDULING HELPER
// ==========================================================================
//
// The eligibility math used to live inline inside `run()` AND as a
// separate test-only `compute_schedule_for_title`. Two copies of the
// same logic meant any future change had to be applied in both
// places or they'd silently diverge. Now unified — `run()` calls
// `compute_schedule_for_title` too, so there's exactly one definition
// of "when should we try to fire X?" and it's unit-testable without a
// tokio runtime or real XboxClient. Audit bug #12.
