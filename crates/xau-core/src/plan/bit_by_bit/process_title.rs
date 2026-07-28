//! The per-title unlock loop: `process_title` (probe-first, pace, gate, POST), its `TitleOutcome`/`TitleResult`, and the transient-retry unlock.

use super::*;

pub(super) struct TitleOutcome {
    /// Numeric Xbox title id — carried so a skipped/impossible title can
    /// be REGISTERED with its id (spec item 1), not just its name.
    pub(super) title_id: String,
    pub(super) name: String,
    pub(super) result: TitleResult,
}

pub(super) enum TitleResult {
    Unlocked {
        count: usize,
        gamerscore: i32,
    },
    /// Event-based title where some achievements landed but
    /// others didn't. Distinct from `Unlocked` so the drain loop
    /// can CREDIT the successes AND ALSO surface the failures —
    /// previously partial event success folded into `Unlocked`
    /// which cleared `last_error` and hid the failures entirely.
    Partial {
        count: usize,
        gamerscore: i32,
        failed: usize,
        message: String,
    },
    EventBasedSkipped,
    /// The account's token is not authorized to write achievements for
    /// this title — Xbox rejects every unlock POST with HTTP 403
    /// `code:21 "The request's security claims were rejected."`. This
    /// is an auth-scope wall (the token lacks the per-title claim), not
    /// a transient error: EVERY achievement in the title fails the same
    /// way, so the runner early-aborts after a few and records the
    /// title here instead of hammering all N. Registered distinctly so
    /// the report can tell the user "these games can't unlock with this
    /// token" separately from real errors.
    SkippedUnauthorized,
    Error(String),
}

/// True if an error string is the HTTP 403 `code:21` "security claims
/// were rejected" rejection — the per-title auth-claim wall. Matches
/// the JSON code and the human description defensively (whitespace /
/// body-shape variants).
pub(super) fn is_security_claims_error(msg: &str) -> bool {
    // Match the unique human description, or the JSON `code` field
    // terminated by `,`/`}` so `"code":210` (or 211, …) can't
    // false-positive on a `"code":21` substring.
    msg.contains("security claims were rejected")
        || msg.contains("\"code\":21,")
        || msg.contains("\"code\": 21,")
        || msg.contains("\"code\":21}")
        || msg.contains("\"code\": 21}")
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn process_title(
    client: &XboxClient,
    my_xuid: &str,
    title_id: String,
    game_name: String,
    scid: String,
    // Was `Vec<String>` + a separate `expected_gs: i32`. The old
    // split discarded per-achievement gamerscore, which made the
    // event-path partial-success credit use a naive proportion
    // (`succeeded/attempted * expected_gs`). That's wrong whenever
    // achievements in the same title have different point values:
    // e.g. a 100G success + 5G failure was reported as 52G.
    // Carry the per-ID gamerscore + scheduled_unix so process_title
    // can (a) sum only what actually landed, and (b) sleep until
    // each entry's scheduled time before posting — honouring the
    // target's pacing when the user opts into a multi-entry time
    // budget. Audit bug BBB-Timing#4/#9.
    //
    // Tuple shape: `(achievement_id, gamerscore, scheduled_unix_secs,
    // scheduled_subsec_ms)`. The 4th element is the SOURCE entry's
    // sub-second offset (`time_unlocked = "...:25.737Z"` → `737`)
    // preserved verbatim from the plan — the sleep helper
    // multiplies `scheduled_unix * 1000 + scheduled_subsec_ms` to
    // hit the original sub-second cadence for same-second bursts.
    // See `BBB-Subsec` on `ScheduledEntry`.
    eligible: Vec<(String, i32, i64, u16)>,
    chunk_size_override: Option<usize>,
    safety: crate::plan::safety::SafetyPolicy,
    // Semaphore from the caller — we acquire a permit JUST BEFORE
    // each unlock POST and release on POST return. Sleeping does
    // NOT hold a permit, so long waits for future scheduled times
    // don't block other titles' concurrent work. See audit bug
    // BBB-Permit-During-Sleep#2.
    sem: Arc<tokio::sync::Semaphore>,
    // Presence-ceiling gate: a permit is acquired up-front and held for
    // this title's WHOLE lifetime (unlike `sem`, which is per-POST), so
    // no more than MAX_SIMULTANEOUS_TITLES (8) titles broadcast
    // presence — via the per-title heartbeat below — at once. Xbox
    // drops presence past 8 and event-based unlocks then fail. See
    // audit: unbounded-concurrent-title-activation.
    activation_sem: Arc<tokio::sync::Semaphore>,
    // Cancellation signal — cloned from the run-level
    // `watch::Receiver<bool>` so sleeps and per-POST loops can
    // check / await cancellation without blocking on `join_next`
    // boundaries. See audit bug BBB-Sleep-Cancel#1.
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    // Shared progress so this worker can push/pop its own title
    // into `active_titles` (drop-guard pattern below). Lets the
    // GUI render "currently processing: A, B, C" mid-run.
    shared: Arc<RwLock<BitByBitProgress>>,
    // Shared DYNAMIC presence set (title_ids). This worker inserts its
    // title_id right after acquiring the activation permit and removes
    // it on every exit path (same drop-guard discipline as
    // `active_titles`), so the keepalive loop heartbeats only titles
    // that are actually being processed — presence rotates as titles
    // start/finish, never exceeding MAX_SIMULTANEOUS_TITLES.
    active_title_ids: Arc<std::sync::Mutex<Vec<String>>>,
    // This run's id — stamped on every wire event so the shared history
    // is filterable per run and mirrored into the run's own log file.
    run_id: String,
    // Shared proactive rate pacer — acquired before every unlock POST so
    // the aggregate rate across all workers stays under Xbox's budget.
    unlock_gate: Arc<tokio::sync::Mutex<Instant>>,
    // Per-game minimum plausible completion time (seconds). `> 0` only
    // in sequential mode: the game's not-already-done unlocks are then
    // spread EVENLY across this many seconds (measured from the moment
    // the "playing" heartbeat goes out), so the last one — reaching
    // 100% — fires no earlier than `start + floor_secs`. Overrides the
    // per-entry gap_mode schedule for the active game. `0` = concurrent
    // mode: honor the scheduled_unix timings verbatim as before.
    floor_secs: u64,
    // How much of `floor_secs` is still OUTSTANDING — the total minus
    // whatever Xbox already credits me on this title. Equal to
    // `floor_secs` for a game starting fresh; smaller when resuming one
    // that was partly done. Drives the PACING; the gate keeps using
    // `floor_secs`, because that is an absolute total Xbox must show,
    // not an amount of work left to do. Spreading unlocks across the
    // full floor on every restart was what made a resumed game claim
    // its whole completion time again.
    pace_secs: u64,
) -> TitleOutcome {
    // Acquire the presence-ceiling permit BEFORE touching `active_titles`
    // or firing any heartbeat, and hold it until this function returns.
    // At most MAX_SIMULTANEOUS_TITLES titles get past this point
    // concurrently; the rest of the spawned tasks park here until an
    // active title finishes. Cancellation is still observed: if the run
    // is stopping, `acquire_owned` may resolve after permits free, and
    // the stop checks below short-circuit the work. A closed semaphore
    // (dropped `run`) yields `Err` → treat as cancelled.
    let _activation_permit = match activation_sem.acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return TitleOutcome {
                title_id: title_id.clone(),
                name: game_name,
                result: TitleResult::Unlocked {
                    count: 0,
                    gamerscore: 0,
                },
            }
        }
    };
    // BUG HUNTER M-bbb-active-titles: drop-guard so a title is
    // ALWAYS removed from `active_titles` on every exit path
    // (success, error, panic, cancel). Without the guard, a
    // panicking title would leave its name in `active_titles`
    // forever and the GUI would mis-display it as "still being
    // processed" until run end.
    struct ActiveGuard {
        shared: Arc<RwLock<BitByBitProgress>>,
        name: String,
        /// Also owns this title's entry in the per-game live list, so
        /// the two can never drift apart on an error/cancel/panic exit.
        title_id: String,
    }
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            {
                let mut w = self
                    .shared
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Remove the FIRST occurrence (we may have duplicates
                // if a future refactor processes the same title twice).
                if let Some(pos) = w.active_titles.iter().position(|n| n == &self.name) {
                    w.active_titles.swap_remove(pos);
                }
            }
            remove_active_live(&self.shared, &self.title_id);
        }
    }
    {
        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Use the human-readable game_name where available so the GUI
        // shows "Pillars of Eternity 2 - PC" not "1742482395". Fall back
        // to title_id when the plan didn't carry a game_name.
        let display = if game_name.is_empty() {
            title_id.clone()
        } else {
            game_name.clone()
        };
        w.active_titles.push(display);
    }
    let _active_guard = ActiveGuard {
        shared: shared.clone(),
        name: if game_name.is_empty() {
            title_id.clone()
        } else {
            game_name.clone()
        },
        title_id: title_id.clone(),
    };
    // Register this title in the DYNAMIC presence set so the keepalive
    // loop broadcasts it, and de-register on EVERY exit path (success,
    // error, cancel, panic) via the drop guard — so when this title
    // finishes, the next tick stops refreshing its presence and Xbox
    // lets it lapse, freeing a slot. Mirrors the `ActiveGuard` above.
    struct PresenceMembership {
        set: Arc<std::sync::Mutex<Vec<String>>>,
        title_id: String,
    }
    impl Drop for PresenceMembership {
        fn drop(&mut self) {
            let mut g = self
                .set
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(pos) = g.iter().position(|t| t == &self.title_id) {
                g.swap_remove(pos);
            }
        }
    }
    {
        let mut g = active_title_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !g.iter().any(|t| t == &title_id) {
            g.push(title_id.clone());
        }
    }
    let _presence_membership = PresenceMembership {
        set: active_title_ids.clone(),
        title_id: title_id.clone(),
    };
    // Xbox presence is LAST-WRITE-WINS (docs/ARCHITECTURE.md Rule 9):
    // a heartbeat ASSERTS the full set of titles you are present in,
    // it does not add one. With a sequential window > 1 (or in
    // concurrent mode) several titles are live at once, so
    // heartbeating only our own id would silently drop every sibling
    // until the global keepalive's next tick re-asserted them — and
    // MinutesPlayed only accrues while present, which is exactly what
    // the floor gate polls. So always heartbeat the whole live set.
    // `PresenceMembership` above has already inserted us, so the
    // snapshot contains this title; the fallback is pure paranoia.
    let presence_set = || -> Vec<String> {
        let live = active_title_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if live.is_empty() {
            vec![title_id.clone()]
        } else {
            live
        }
    };
    // Derive the id-only view for callers (safety gate, batch POST)
    // and a per-id gamerscore lookup so we can recompute the total
    // AFTER the safety gate runs. `eligible` is small (typical
    // title: 10–50 entries), so a HashMap is fine.
    //
    // BUG HUNTER BBB-Order-Invariant: `eligible` arrives from
    // `compute_schedule_for_title` (or `compute_tiered_schedule`)
    // followed by a `take(dispatch_budget_remaining)` slice in
    // `run()`. Both schedulers produce a Vec sorted by
    // `scheduled_unix`. The runner's per-id POST loop iterates this
    // Vec in input order, so wire-order = `eligible` order. Lock
    // that invariant at the boundary: a debug-build assertion
    // catches any future scheduler refactor that silently breaks
    // the "monotonic non-decreasing scheduled_unix" guarantee
    // before it reaches users.
    //
    // Connection affinity note (BBB-Connection-Affinity):
    // `XboxClient` holds a single `reqwest::Client` whose pool
    // (`pool_max_idle_per_host=128`, `tcp_keepalive=60s`) hands the
    // same idle socket back to sequential `.send()` calls from this
    // async task — under realistic plan timings (Capped/Tiered with
    // gaps below `pool_idle_timeout=90s`) the entire per-title POST
    // sequence reuses one TCP connection, so any Xbox shard that
    // pins by connection sees this title's POSTs in arrival order.
    // For Tiered modes whose deepest gaps exceed 90s of wall time,
    // the relative order constraint at sub-millisecond resolution
    // becomes moot anyway — the next POST is hours away on Xbox's
    // own clock. Per-title pinning was analysed and rejected: the
    // e2e wire-order test (`tests/bbb_wire_order_e2e.rs`)
    // empirically proves arrival order matches plan order across 3
    // concurrent titles with the existing pool setup.
    debug_assert!(
        eligible.windows(2).all(|w| {
            // Lexicographic compare on (secs, subsec_ms) — same
            // chronological order the source plan used after the
            // BBB-Timing-1 stable sort.
            (w[0].2, w[0].3) <= (w[1].2, w[1].3)
        }),
        "process_title precondition broken: `eligible` must be sorted by \
         (scheduled_unix, scheduled_subsec_ms) non-decreasing — scheduler/dispatch \
         produced out-of-order entries for title `{title_id}`. Wire-order would \
         not match plan chronology.",
    );
    // BUG HUNTER M-subsec-domain: `scheduled_subsec_ms` is a u16 in
    // the [0, 999] domain by construction — `compute_schedule_for_title`
    // and `compute_tiered_schedule` derive it via `rem_euclid(1000)`.
    // A future scheduler refactor that swaps in a buggy modulo (or
    // forgets to clamp) would let through 1000+ values, which would
    // break the wire-time comparison at `secs * 1000 + subsec_ms` and
    // silently shift POSTs by a full second. Catch any such regression
    // at debug time — invariant is a single AND, basically free.
    debug_assert!(
        eligible
            .iter()
            .all(|(_, _, _, subsec_ms)| *subsec_ms < 1000),
        "process_title precondition broken: `scheduled_subsec_ms` must be < 1000; \
         scheduler produced an out-of-domain value for title `{title_id}`."
    );
    let eligible_ids: Vec<String> = eligible.iter().map(|(id, _, _, _)| id.clone()).collect();
    // UI/perf wave: FxHashMap (rustc-hash) instead of std HashMap.
    // FxHash is ~2× faster than SipHash on short string keys, and
    // both `gs_by_id` and `scheduled_by_id` are LOCAL maps with
    // server-controlled keys (Xbox achievement_ids) — no DoS surface
    // that justifies SipHash's collision-resistance. The maps are
    // hit once per achievement during the per-title unlock loop, so
    // a 100-achievement title saves ~100 hash computations × the
    // SipHash/FxHash delta per run.
    let gs_by_id: rustc_hash::FxHashMap<String, i32> = eligible
        .iter()
        .map(|(id, gs, _, _)| (id.clone(), *gs))
        .collect();
    // Map `achievement_id → (scheduled_unix_secs, scheduled_subsec_ms)`.
    // The pair is what the sleep helper needs — combined into ms via
    // `secs * 1000 + subsec_ms` at the call site so a same-second
    // source burst fires at its original sub-second cadence.
    let scheduled_by_id: rustc_hash::FxHashMap<String, (i64, u16)> = eligible
        .iter()
        .map(|(id, _, sched, subsec)| (id.clone(), (*sched, *subsec)))
        .collect();
    // =============== PER-TITLE PRESENCE HEARTBEAT ==================
    //
    // Presence is scoped to THIS title_id (the global
    // `presence_keepalive_loop` only snapshots the first
    // `effective_concurrency` title_ids at spawn, so a rotated-in
    // title needs its own heartbeat to show "playing X"). The WHEN
    // is the lazy model (see the big comment at the up-front
    // heartbeat above): in the sequential per-game path we send it
    // only AFTER the first unlock lands, so a dead / event-based
    // title costs zero presence traffic. The up-front heartbeat only
    // fires for `is_event` titles (OneCollector needs active presence
    // to commit) and concurrent mode (no probe-first there). See
    // audit bug BBB-Presence#1/#8. A transient heartbeat failure is
    // logged at debug and the unlock proceeds anyway.
    // BUG HUNTER BBB-PreflightPermit: previously a `sem.acquire_owned()`
    // permit was held across BOTH the per-title heartbeat AND the
    // safety-gate GET. With concurrency=6 on a 9k-title plan, the
    // very first batch hit Xbox's rate-limit gate during safety GETs
    // and parked there for up to 300s WHILE STILL HOLDING the permit.
    // No new title could be dispatched, no per-POST permit could be
    // issued, and the user saw "state=Running" for minutes with zero
    // unlocks landing. Removing the preflight permit entirely is the
    // right call: the rate limiter at the HTTP retry layer
    // already throttles concurrent calls per endpoint, so the
    // permit-bound concurrency cap was redundant for these read paths.
    // The PER-POST permit (acquired just before each unlock POST in
    // the loop below) still bounds the actual mutating work to
    // `config.title_concurrency`.
    // PRESENCE, LAZILY (the user's insight): you CANNOT know a title is
    // event-based up front — the only reliable signal is a `code:49` on
    // the first direct unlock. So in the sequential per-game path we send
    // ZERO presence up front: the loop below PROBES the first achievement
    // with no presence, and only AFTER one actually lands (proving the
    // title is a normal, completable game) do we send the heartbeat (see
    // `presence_established`). A dead / event-based title thus costs ZERO
    // presence traffic — it just fails the probe and is skipped.
    //
    // Two exceptions still heartbeat up front (they don't probe-first):
    //   - `is_event` (the events bundle already LISTS this title — a
    //     known fact, not a guess): the OneCollector path needs presence
    //     to commit, so send it now and route to the event loop below.
    //   - Concurrent mode (`floor_secs == 0`, incl. the bulk chunk path):
    //     no probe-first there, so keep the one up-front heartbeat that
    //     established the "playing X" profile record.
    let is_event = client.is_event_based_title(&title_id);
    if is_event || floor_secs == 0 {
        if let Err(e) = client.send_heartbeat_bulk(my_xuid, &presence_set()).await {
            tracing::debug!(
                ?e,
                title_id = %title_id,
                "per-title presence heartbeat failed; unlock may reject if event-based"
            );
        }
    }
    // Sequential-mode anchor: the instant this game went "online"
    // (i.e. now — right before the probe). Per-game elapsed time is
    // measured from here, so the floor pacing (`floor_adjusted_target`)
    // spreads this game's unlocks across `floor_secs` from this anchor.
    // Presence itself is sent lazily (after the first landed unlock),
    // so the anchor is the go-online instant, not the heartbeat instant.
    let title_start_ms = wallclock_ms();
    // Publish the game now "online"/unlocking so the UI can show
    // "▶ NOW: <game>" while it's active (last_title only lands at
    // completion). Cleared at terminal in reconcile_terminal_state.
    {
        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.active_game_name = Some(game_name.clone());
    }
    // ============= UNLOCK-SAFETY GATE #2 (bit_by_bit) ==============
    // Plans can be reloaded from disk; a plan built before the
    // safety filter landed could still contain revoked or
    // expired entries. Re-check here with fresh metadata.
    let eligible_ids = match crate::plan::safety::filter_unlock_safe(
        client,
        my_xuid,
        &title_id,
        &game_name,
        &eligible_ids,
        safety,
    )
    .await
    {
        Ok((safe, _skipped)) => safe,
        Err(e) => {
            return TitleOutcome {
                title_id: title_id.clone(),
                name: game_name,
                result: TitleResult::Error(format!("safety gate: {e}")),
            };
        }
    };
    if eligible_ids.is_empty() {
        // Everything was filtered by the safety gate. Count as an
        // unlocked-0 rather than an error so the title doesn't
        // inflate the error counter. The audit log tells the
        // real story.
        return TitleOutcome {
            title_id: title_id.clone(),
            name: game_name,
            result: TitleResult::Unlocked {
                count: 0,
                gamerscore: 0,
            },
        };
    }
    // Recompute `expected_gs` AFTER the safety gate — previously this
    // was computed over the pre-filter set and the "all succeeded"
    // branches below credited gamerscore for achievements that were
    // never actually attempted. Bug #7 from the audit: safety filter
    // removed a 50G achievement → HUD still reported the 50G as
    // earned. See audit trail + unit test
    // `safety_gate_filters_does_not_credit_removed_score`.
    //
    // Defensive lookup (audit bug BBB-#11): `gs_by_id` was built
    // from the PRE-filter `eligible` slice. Any id that survives
    // the safety gate MUST be present in the map — the gate only
    // drops ids, never mutates or adds them. If a lookup misses
    // that invariant is broken; surface a warn log so we can
    // diagnose rather than silently crediting 0G (which would
    // under-report progress to the user). `unwrap_or(0)` is the
    // safe fallback; the warn makes the rare case auditable.
    // Denominator for the sequential floor pacing: the number of
    // achievements that will ACTUALLY fire this run (post-safety-gate,
    // not-already-done). The floor is spread over exactly these, so
    // a resumed title with fewer remaining still finishes at ~floor.
    let n_eligible = eligible_ids.len();
    // Sequential pacing targets that REPLAY the speedrunner's real
    // intra-title cadence (bursts + gaps), not an even metronome — the
    // fidelity audit's #1 bot-tell fix. Precomputed once per title so
    // both the event-based and per-id loops index the same schedule;
    // in concurrent mode (floor_secs == 0) each entry falls back to its
    // verbatim plan target. Indexed by the same `idx` the loops use.
    let floor_targets: Vec<Option<(i64, u16)>> = {
        let sched: Vec<Option<(i64, u16)>> = eligible_ids
            .iter()
            .map(|id| scheduled_by_id.get(id).copied())
            .collect();
        // PACING uses the OUTSTANDING window, not the total floor —
        // see `pace_secs`. Identical for a fresh game.
        floor_adjusted_targets(&sched, pace_secs, title_start_ms)
    };
    // Publish the active game's per-title progress denominators so the
    // UI can show a secondary bar ("🎮 <game> · 3/34 · ~2h to 100%")
    // during the hours one game holds a sequential run.
    {
        let mut w = shared
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        w.active_title_total = n_eligible;
        w.active_title_done = 0;
        // The card says "time to 100%", which is what REMAINS.
        w.active_title_floor_secs = pace_secs;
    }
    // Per-game card. Carries the same numbers, but keyed by title, so a
    // window > 1 renders one card per game instead of the tasks
    // overwriting each other's single slot. `started_ms` + the pacing
    // window let the GUI run its own countdown and bar with no extra
    // traffic — and it counts down the time that is actually LEFT, so a
    // resumed game no longer shows its whole completion time again.
    push_active_live(
        &shared,
        &title_id,
        &game_name,
        n_eligible,
        pace_secs,
        title_start_ms,
    );
    let expected_gs: i32 = eligible_ids
        .iter()
        .map(|id| match gs_by_id.get(id) {
            Some(gs) => *gs,
            None => {
                tracing::warn!(
                    title_id = %title_id,
                    ach_id = %id,
                    "gs_by_id lookup miss after safety gate — ID normalisation mismatch? \
                     Crediting 0G for this achievement to avoid over-reporting."
                );
                0
            }
        })
        .sum();

    // Event-based titles: delegate to `unlock_achievement` per ID (the
    // per-ID path auto-routes to OneCollector). Any other path would
    // return code:49 and the batch would be marked skipped.
    if is_event {
        // Validate ONCE per title. Typed API at unlock_achievement
        // prevents the arg-order class of bugs at compile time.
        let title_id_t = match crate::types::TitleId::try_from(title_id.as_str()) {
            Ok(v) => v,
            Err(e) => {
                return TitleOutcome {
                    title_id: title_id.clone(),
                    name: game_name,
                    result: TitleResult::Error(format!("bad title_id: {e}")),
                };
            }
        };
        let xuid_t = match crate::types::Xuid::try_from(my_xuid) {
            Ok(v) => v,
            Err(e) => {
                return TitleOutcome {
                    title_id: title_id.clone(),
                    name: game_name,
                    result: TitleResult::Error(format!("bad xuid: {e}")),
                };
            }
        };
        let mut succeeded = 0;
        // Sum the gamerscore of achievements that actually landed,
        // NOT `succeeded / attempted * expected_gs`. The proportion
        // formula reported a 100G success + 5G failure as 52G
        // instead of the correct 100G — the HUD and run totals
        // were lying whenever a title mixed point values.
        let mut succeeded_gs: i32 = 0;
        let mut last_error = None;
        // Consecutive-failure early-abort (mirrors the per-id path): a
        // dead event title fails every POST, so bail after a few instead
        // of walking all N. Combined with probe-until-first-success
        // (below) a dead event title dies in ~3 quick POSTs, no pacing.
        const EVENT_ABORT_THRESHOLD: usize = 3;
        let mut consecutive_fail: usize = 0;
        for (idx, id) in eligible_ids.iter().enumerate() {
            // Early exit on cancel — no point starting a new entry
            // after the user clicked Stop. See BBB-Sleep-Cancel#1.
            if *cancel_rx.borrow() {
                break;
            }
            // BBB-EventEarlyAbort: give up on a title that fails every
            // probe (no successes) after the threshold — don't POST all N.
            if succeeded == 0 && consecutive_fail >= EVENT_ABORT_THRESHOLD {
                tracing::info!(
                    title_id = %title_id,
                    consecutive_fail,
                    "event-based title: aborting after consecutive failures with zero successes"
                );
                break;
            }
            let id_t = match crate::types::AchievementId::try_from(id.as_str()) {
                Ok(v) => v,
                Err(e) => {
                    last_error = Some(format!("bad achievement_id `{id}`: {e}"));
                    continue;
                }
            };
            // Honour the target's pacing: sleep until this entry's
            // scheduled_unix before posting. When the user runs
            // bit-by-bit with `time_budget=None` (the common case)
            // everything eligible has `scheduled_unix <= now`, so
            // the sleep is a no-op. With a non-None budget, entries
            // in [now, now + budget] sleep the appropriate delta —
            // producing Xbox server timestamps that match the
            // target's original cadence instead of all collapsing
            // to a single moment. Audit bug BBB-Timing#4/#9.
            //
            // `sleep_until_scheduled_cancellable` races the sleep
            // against `cancel_rx.changed()` so Stop interrupts
            // in-flight sleeps within ms, not at the next `join_next`
            // boundary (which may be hours away). See
            // BBB-Sleep-Cancel#1.
            // In sequential mode (`floor_secs > 0`) this REPLACES the
            // plan's per-entry schedule with an even spread across the
            // per-game floor; in concurrent mode it returns the plan
            // target unchanged. Either way the sleep helper keeps the
            // cancel race + long-wait presence refresh.
            let target = floor_targets.get(idx).copied().flatten();
            // PROBE-UNTIL-FIRST-SUCCESS (sequential mode only): fire
            // achievements back-to-back with NO pacing sleep and NO floor
            // gate until ONE actually lands. Only AFTER the title proves
            // itself live (>=1 success) do we start pacing the remaining
            // unlocks across the floor + gating the last one.
            //
            // Why: a dead title (event-based / code:49) unlocks nothing,
            // so pacing it is pure wasted wall-clock. The old "pace when
            // idx > 0" meant a dead 10-cheevo game fired #0 fast, then
            // SLEPT ~5-6 min to #1's cadence target before failing again —
            // the "0/10 · next unlock in 5m 56s" the user saw. Gating on
            // "any success yet?" instead of "idx > 0" makes a dead title
            // burn through its abort threshold in seconds, while a LIVE
            // title is unchanged (its #0 lands, then #1.. pace as before).
            //
            // CONCURRENT mode (floor_secs == 0) always keeps its
            // time_budget-scheduled sleep (`|| floor_secs == 0`) so Xbox
            // timestamps match the target's pacing (BBB-Timing#4/#9).
            //
            // Accepted trade-off (user's choice): a LIVE single-remaining
            // title reaches 100% at game-start with its floor relaxed.
            if floor_secs == 0 || succeeded > 0 {
                // Publish WHEN the next unlock fires so the GUI can show
                // "next unlock in 12m" during long sequential sleeps
                // instead of looking hung (spec item 2).
                publish_next_unlock_at(&shared, target);
                if sleep_until_scheduled_cancellable(
                    target,
                    client,
                    my_xuid,
                    &title_id,
                    &presence_set(),
                    &mut cancel_rx,
                )
                .await
                .is_err()
                {
                    break; // cancelled mid-sleep (or run state torn down)
                }
            } else {
                // Probe fires now — no unlock is "scheduled for later".
                publish_next_unlock_at(&shared, None);
            }
            // PRECISION GATE (sequential): the FINAL achievement of a
            // MULTI-achievement game — the one that flips it to 100% — is
            // ALWAYS gated on OUR real MinutesPlayed reaching the floor,
            // whether or not we paced to it. Placing this OUTSIDE the pace
            // branch + gating on `idx > 0` closes the hole where a
            // probe-until-success run whose first landing is the last
            // eligible achievement would reach 100% ungated. `idx > 0`
            // means "the last but NOT also the first" — a single-remaining
            // title (idx 0 == last) keeps the accepted probe relaxation
            // (fires at game-start; dead ones die fast). Never faster to
            // 100% than the fastest TA speedrunner on the true stat.
            if floor_secs > 0 && idx > 0 && idx + 1 == n_eligible {
                match await_playtime_floor(
                    client,
                    my_xuid,
                    &title_id,
                    floor_secs,
                    &shared,
                    &mut cancel_rx,
                    &active_title_ids,
                )
                .await
                {
                    Err(()) => break, // cancelled while waiting on the floor
                    Ok(outcome) => record_floor_gate_outcome(
                        &shared, &run_id, &title_id, &game_name, floor_secs, &outcome,
                    ),
                }
            }
            // Acquire permit JUST BEFORE the POST — released on drop
            // at scope exit. Sleeping above held no permit, so other
            // titles' concurrent work wasn't blocked. See
            // BBB-Permit-During-Sleep#2.
            let Ok(_permit) = sem.clone().acquire_owned().await else {
                break; // semaphore closed (shouldn't happen)
            };
            // BUG HUNTER BBB-WireLog: capture `ts_sent` immediately
            // before the POST is dispatched and `ts_resp` immediately
            // after the response (success or fail). The pair lets a
            // post-hoc audit prove the wire-order matches the plan
            // chronology — a true forensic trail rather than just a
            // success/fail count.
            // Proactive rate-limit pace: reserve this POST's slot so the
            // aggregate rate across workers can't exceed Xbox's budget.
            // Sleeping here (before ts_sent) means drift reflects the real
            // send time, and the run can NEVER 429-fail from overflow.
            pace_unlock(&unlock_gate).await;
            let ts_sent_ms = wallclock_ms();
            let attempt =
                unlock_with_transient_retry(client, &title_id_t, &xuid_t, &id_t, &mut cancel_rx)
                    .await;
            let ts_resp_ms = wallclock_ms();
            match attempt {
                UnlockAttempt::Success(_) => {
                    succeeded += 1;
                    consecutive_fail = 0;
                    // Same invariant as the `expected_gs` recompute
                    // above: every id in `eligible_ids` was in the
                    // pre-filter `eligible` slice, so the lookup
                    // should always hit. Log on miss so the rare
                    // breakage is auditable instead of silently
                    // shipping a 0G credit to the UI. See audit bug
                    // BBB-#11.
                    let this_gs = gs_by_id.get(id).copied();
                    match this_gs {
                        Some(gs) => {
                            succeeded_gs = succeeded_gs.saturating_add(gs);
                        }
                        None => {
                            tracing::warn!(
                                title_id = %title_id,
                                ach_id = %id,
                                "gs_by_id lookup miss after successful unlock — succeeded but won't credit gamerscore"
                            );
                        }
                    }
                    // Live per-achievement tick + feed line for the dock.
                    bump_live_unlock(&shared, id, &title_id, &game_name, this_gs.unwrap_or(0));
                    log_wire_event(
                        &title_id,
                        id,
                        scheduled_by_id.get(id).copied(),
                        target,
                        ts_sent_ms,
                        ts_resp_ms,
                        "success",
                        None,
                        my_xuid,
                        &run_id,
                        &game_name,
                    );
                }
                UnlockAttempt::Cancelled => {
                    // BBB-1: a Stop click during the retry cool-off is
                    // NOT an unlock failure. Break out of the loop
                    // exactly like the top-of-iteration cancel check
                    // would, leaving `last_error` untouched and
                    // crediting `succeeded` honestly.
                    log_wire_event(
                        &title_id,
                        id,
                        scheduled_by_id.get(id).copied(),
                        target,
                        ts_sent_ms,
                        ts_resp_ms,
                        "cancelled",
                        None,
                        my_xuid,
                        &run_id,
                        &game_name,
                    );
                    break;
                }
                UnlockAttempt::Failed(e) => {
                    consecutive_fail = consecutive_fail.saturating_add(1);
                    tracing::warn!(title_id = %title_id, ach_id = %id, ?e, "bit-by-bit event unlock failed");
                    let msg = e.to_string();
                    log_wire_event(
                        &title_id,
                        id,
                        scheduled_by_id.get(id).copied(),
                        target,
                        ts_sent_ms,
                        ts_resp_ms,
                        "failed",
                        Some(&msg),
                        my_xuid,
                        &run_id,
                        &game_name,
                    );
                    last_error = Some(msg);
                }
            }
        }
        if succeeded == 0 {
            return TitleOutcome {
                title_id: title_id.clone(),
                name: game_name,
                result: TitleResult::Error(format!(
                    "event-based unlock failed for all {} achievement(s){}",
                    eligible_ids.len(),
                    last_error
                        .as_deref()
                        .map_or_else(String::new, |msg| format!(": {msg}"))
                )),
            };
        }
        // All-succeeded path is the common case for events (the
        // OneCollector path is nearly all-or-nothing). When every
        // attempted id landed, report the full expected_gs so the
        // HUD shows the exact library-progress delta the user
        // expects; partial branches into `Partial` so the drain
        // loop can credit successes AND surface the failures.
        if succeeded == eligible_ids.len() {
            return TitleOutcome {
                title_id: title_id.clone(),
                name: game_name,
                result: TitleResult::Unlocked {
                    count: succeeded,
                    gamerscore: expected_gs,
                },
            };
        }
        let failed = eligible_ids.len() - succeeded;
        let message = format!(
            "event-based unlock only {succeeded}/{} landed ({failed} failed){}",
            eligible_ids.len(),
            last_error
                .as_deref()
                .map_or_else(String::new, |msg| format!("; last error: {msg}"))
        );
        return TitleOutcome {
            title_id: title_id.clone(),
            name: game_name,
            result: TitleResult::Partial {
                count: succeeded,
                gamerscore: succeeded_gs,
                failed,
                message,
            },
        };
    }

    // Non-event title: resolve scid if we don't have one, then batch-POST.
    // ZERO_UID ("00000000-…") is the serde default for `service_config_id`
    // on Achievement payloads, so plan files built from responses where
    // Xbox omitted the field will carry ZERO_UID instead of empty. Treat
    // both as "unresolved" here — otherwise we'd POST to the URL
    // `/achievements/00000000-…-0000/update` and Xbox returns 404, which
    // the runner reports as "Error" for the entire title. See audit bug
    // BBB-Unlock#3 + client.rs `warm_scid_cache_bulk` which already
    // treats ZERO_UID as missing.
    let scid_is_missing = scid.is_empty() || scid == crate::xbox::constants::ZERO_UID;
    let effective_scid = if scid_is_missing {
        match client.resolve_service_config_id(my_xuid, &title_id).await {
            Ok(s) => s,
            Err(e) => {
                return TitleOutcome {
                    title_id: title_id.clone(),
                    name: game_name,
                    result: TitleResult::Error(format!("scid resolve: {e}")),
                };
            }
        }
    } else {
        scid
    };

    // Chunk-size policy (audit bug BBB-Timing/Forensics):
    //
    // Bit-by-bit's whole point is replicating the TARGET's chronology
    // with per-achievement timestamps that look naturally paced on
    // the user's Xbox profile. A chunk of size N POSTs N achievements
    // in ONE HTTP call, and Xbox stamps all N with the SAME
    // server-side `timeUnlocked` — so a forensic timeline-analysis
    // would see "user unlocked 64 achievements in the same
    // millisecond", which is an obvious red flag.
    //
    // So the default here is `chunk_size=1` (one POST per
    // achievement, each gets its own timestamp a few hundred ms
    // apart). Callers who explicitly opt into bulk chunking for
    // speed can override via `chunk_size_override = Some(N)`. The
    // `--strict-order` CLI flag and the GUI "strict chronology"
    // toggle wire through to the explicit-1 path; omitting those
    // USED to fall back to `UNLOCK_CHUNK_SIZE` and destroy
    // chronology. We now treat the fallback as "strict by default,
    // opt-in to speed".
    // Sequential floor pacing needs the per-id path to spread unlocks
    // over time — the bulk chunk path fires everything in one POST and
    // couldn't honor the floor. Force chunk_size=1 whenever a floor is
    // set, overriding any caller opt-in to bulk.
    let chunk_size = if floor_secs > 0 {
        1
    } else {
        chunk_size_override.unwrap_or(1)
    };
    // When chunk_size == 1 we use the per-id iteration path so we
    // can honor `scheduled_unix` per entry (sleep until each target
    // time before posting) — forensic-perfect timing. For chunk_size
    // > 1 the caller explicitly opted into bulk POSTing at the cost
    // of chronology, so we fall back to the detailed-chunked call
    // which skips per-entry sleeps. Audit bug BBB-Timing#4/#9.
    let summary = if chunk_size == 1 {
        let mut acc = crate::xbox::UnlockChunkSummary::default();
        // BUG HUNTER M-bbb-event-early-abort: when a non-event-bundle
        // title is actually event-driven on Xbox's side (rare but
        // real — bundle classification is per-title, but Xbox
        // sometimes treats individual achievements as event-only),
        // every per-id POST returns HTTP 400 with `{"code":49,
        // "description":"None of the submitted achievements may be
        // updated in this fashion."}`. The post-loop classifier
        // already handles this by routing the title to
        // `EventBasedSkipped`, but only AFTER iterating every entry
        // and burning that many slots of rate-limit budget. Live
        // 2026-04-28 run on title 158555809 wasted 18 POSTs
        // discovering what 3 would have proven. Early-abort: if we
        // see `EVENT_ABORT_THRESHOLD` consecutive code:49 with zero
        // successes, break out of the loop. The post-loop classifier
        // still flags `EventBasedSkipped` correctly because
        // `acc.last_error` carries the code:49 marker.
        const EVENT_ABORT_THRESHOLD: usize = 3;
        let mut consecutive_code49: usize = 0;
        // Same early-abort idea for the HTTP 403 `code:21` "security
        // claims were rejected" wall: if the token can't write to this
        // title, EVERY achievement fails identically, so bail after a
        // few instead of hammering all N (each burning a rate-limit
        // slot, wall-clock, and — in sequential mode — the presence
        // floor and the 30-min playtime gate). See the field report:
        // one run hit 28k code:21 failures across 1,362 games because
        // this abort didn't exist.
        let mut consecutive_claims: usize = 0;
        // Presence is sent LAZILY on the per-id (non-event) path: no
        // heartbeat went out up front, so a dead/event title (code:49)
        // costs zero presence traffic. The FIRST landed unlock proves the
        // title is a normal, completable game — only then do we send one
        // heartbeat to establish presence for the floor + the "playing X"
        // profile record; the global keepalive loop refreshes it after.
        // Concurrent mode (`floor_secs == 0`) already sent the up-front
        // heartbeat, so it starts "established" and never double-sends.
        let mut presence_established = floor_secs == 0;
        for (idx, id) in eligible_ids.iter().enumerate() {
            // Cancel check before each per-id POST — see
            // BBB-Sleep-Cancel#1.
            if *cancel_rx.borrow() {
                break;
            }
            if consecutive_code49 >= EVENT_ABORT_THRESHOLD && acc.succeeded_ids.is_empty() {
                tracing::info!(
                    title_id = %title_id,
                    consecutive_code49,
                    remaining_eligible = eligible_ids
                        .len()
                        .saturating_sub(acc.chunks_attempted),
                    "BBB-EventEarlyAbort: title appears event-driven \
                     (consecutive code:49 with no successes); skipping \
                     remaining per-id POSTs to preserve rate-limit budget"
                );
                break;
            }
            if consecutive_claims >= EVENT_ABORT_THRESHOLD && acc.succeeded_ids.is_empty() {
                tracing::info!(
                    title_id = %title_id,
                    game = %game_name,
                    consecutive_claims,
                    remaining_eligible = eligible_ids
                        .len()
                        .saturating_sub(acc.chunks_attempted),
                    "BBB-ClaimsEarlyAbort: token not authorized to write \
                     achievements for this title (consecutive code:21 with \
                     no successes); skipping the rest to preserve budget"
                );
                break;
            }
            // Sequential floor pacing (see event-path twin above): even
            // spread across the per-game floor when floor_secs > 0, else
            // the verbatim plan schedule.
            let target = floor_targets.get(idx).copied().flatten();
            // PROBE-UNTIL-FIRST-SUCCESS (sequential only) — see the
            // event-path twin: fire achievements back-to-back with no
            // pacing/gate until ONE lands, THEN pace the rest + gate the
            // last. A dead title never lands one, so it burns its abort
            // threshold in seconds instead of sleeping to each cadence
            // target. `|| floor_secs == 0` keeps CONCURRENT mode's
            // per-entry time_budget sleep intact.
            if floor_secs == 0 || !acc.succeeded_ids.is_empty() {
                publish_next_unlock_at(&shared, target);
                if sleep_until_scheduled_cancellable(
                    target,
                    client,
                    my_xuid,
                    &title_id,
                    &presence_set(),
                    &mut cancel_rx,
                )
                .await
                .is_err()
                {
                    break; // cancelled mid-sleep (or run state torn down)
                }
            } else {
                publish_next_unlock_at(&shared, None);
            }
            // PRECISION GATE (sequential) — see the event-path twin: the
            // FINAL achievement of a MULTI-achievement game is ALWAYS
            // gated (outside the pace branch, `idx > 0`) so a
            // probe-until-success run whose first landing is the last
            // eligible achievement can't reach 100% ungated; a
            // single-remaining title (idx 0 == last) keeps the probe.
            if floor_secs > 0 && idx > 0 && idx + 1 == n_eligible {
                match await_playtime_floor(
                    client,
                    my_xuid,
                    &title_id,
                    floor_secs,
                    &shared,
                    &mut cancel_rx,
                    &active_title_ids,
                )
                .await
                {
                    Err(()) => break, // cancelled while waiting on the floor
                    Ok(outcome) => record_floor_gate_outcome(
                        &shared, &run_id, &title_id, &game_name, floor_secs, &outcome,
                    ),
                }
            }
            // Permit acquired JUST BEFORE the POST, released on
            // scope exit. See BBB-Permit-During-Sleep#2.
            let Ok(_permit) = sem.clone().acquire_owned().await else {
                break;
            };
            acc.chunks_attempted = acc.chunks_attempted.saturating_add(1);
            // BUG HUNTER BBB-WireLog: same forensic capture as the
            // event-based path — log `ts_sent` / `ts_resp` per POST.
            // Proactive rate-limit pace: reserve this POST's slot so the
            // aggregate rate across workers can't exceed Xbox's budget.
            // Sleeping here (before ts_sent) means drift reflects the real
            // send time, and the run can NEVER 429-fail from overflow.
            pace_unlock(&unlock_gate).await;
            let ts_sent_ms = wallclock_ms();
            let post_result = client
                .unlock_title_based_achievements_detailed(
                    &effective_scid,
                    &title_id,
                    my_xuid,
                    std::slice::from_ref(id),
                    1,
                )
                .await;
            let ts_resp_ms = wallclock_ms();
            match post_result {
                Ok(mut chunk_summary) => {
                    let landed = chunk_summary.succeeded_ids.iter().any(|s| s == id);
                    acc.succeeded_ids.append(&mut chunk_summary.succeeded_ids);
                    let err_str = chunk_summary.last_error.as_ref().map(ToString::to_string);
                    log_wire_event(
                        &title_id,
                        id,
                        scheduled_by_id.get(id).copied(),
                        target,
                        ts_sent_ms,
                        ts_resp_ms,
                        if landed { "success" } else { "failed" },
                        err_str.as_deref(),
                        my_xuid,
                        &run_id,
                        &game_name,
                    );
                    if landed {
                        // Successful unlock — reset BOTH early-abort
                        // counters so a single recoverable failure later
                        // doesn't trip them.
                        consecutive_code49 = 0;
                        consecutive_claims = 0;
                        // FIRST landing proves this is a normal game — NOW
                        // establish presence (skipped up front so a dead/
                        // event title wastes none). One heartbeat; the
                        // global keepalive loop refreshes it for the rest.
                        if !presence_established {
                            presence_established = true;
                            let _ = client.send_heartbeat_bulk(my_xuid, &presence_set()).await;
                        }
                        // Live per-achievement tick + feed line for the dock.
                        bump_live_unlock(
                            &shared,
                            id,
                            &title_id,
                            &game_name,
                            gs_by_id.get(id).copied().unwrap_or(0),
                        );
                    }
                    if let Some(err) = chunk_summary.last_error {
                        // BBB-Timing-2: previously this `break`d the
                        // entire title on any per-id error — a single
                        // retired/withdrawn achievement (400/404) in a
                        // 240k-entry aggregate plan would abort the
                        // whole title, leaving every later eligible id
                        // unrun until the user manually pruned the
                        // plan. The event-based path (line 1532-1535)
                        // already records the failure and continues;
                        // mirror that contract here. We still keep the
                        // most recent error in `acc.last_error` so the
                        // outcome surfaces "Partial" with a hint, but
                        // the rest of the title gets a fair shot.
                        tracing::warn!(
                            title_id = %title_id,
                            ach_id = %id,
                            %err,
                            "bit-by-bit per-id POST returned a permanent error; continuing past it",
                        );
                        let err_msg = err.to_string();
                        // Track consecutive code:49 ("event-only
                        // achievement") errors — same substring set
                        // the post-loop classifier matches. When
                        // these stack up with no successes we break
                        // early via the BBB-EventEarlyAbort gate.
                        if err_msg.contains("\"code\":49")
                            || err_msg.contains("code 49")
                            || err_msg.contains("None of the submitted achievements")
                        {
                            consecutive_code49 = consecutive_code49.saturating_add(1);
                            consecutive_claims = 0;
                        } else if is_security_claims_error(&err_msg) {
                            // HTTP 403 code:21 — the token can't write to
                            // this title. Count toward the claims abort.
                            consecutive_claims = consecutive_claims.saturating_add(1);
                            consecutive_code49 = 0;
                        } else if !landed {
                            // Some other permanent error (e.g. retired
                            // achievement, 404). Don't count toward
                            // either early-abort threshold.
                            consecutive_code49 = 0;
                            consecutive_claims = 0;
                        }
                        acc.last_error = Some(err);
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    log_wire_event(
                        &title_id,
                        id,
                        scheduled_by_id.get(id).copied(),
                        target,
                        ts_sent_ms,
                        ts_resp_ms,
                        "failed",
                        Some(&msg),
                        my_xuid,
                        &run_id,
                        &game_name,
                    );
                    return TitleOutcome {
                        title_id: title_id.clone(),
                        name: game_name,
                        result: TitleResult::Error(format!("precheck: {msg}")),
                    };
                }
            }
        }
        acc
    } else {
        // Opt-in bulk path — no per-entry sleep. Use the detailed
        // variant so chunks that land on Xbox BEFORE a mid-stream
        // failure get credited as `Partial`, not lost in an
        // all-or-nothing `Error`. See audit bug BBB-Unlock#2: the
        // old `_chunked` wrapper returned `Err` on chunk-N failure
        // and the `Ok` arm below then assumed 0 landed on Xbox —
        // but chunks 1..N-1 are already committed server-side, so
        // reporting "Error, 0 unlocks" under-counted up to 63 real
        // unlocks.
        //
        // Permit held only around the chunked POST call itself
        // (which may internally issue multiple HTTP requests, each
        // still serialized through the throttle gate).
        let Ok(_permit) = sem.clone().acquire_owned().await else {
            return TitleOutcome {
                title_id: title_id.clone(),
                name: game_name,
                result: TitleResult::Error("semaphore closed".into()),
            };
        };
        // Pace the batch entry too (this path is only used for
        // chunk_size>1; the bit-by-bit default chunk_size=1 uses the
        // per-id paths above). The batch's internal per-chunk POSTs also
        // pass the reactive throttle gate.
        pace_unlock(&unlock_gate).await;
        match client
            .unlock_title_based_achievements_detailed(
                &effective_scid,
                &title_id,
                my_xuid,
                &eligible_ids,
                chunk_size,
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                return TitleOutcome {
                    title_id: title_id.clone(),
                    name: game_name,
                    result: TitleResult::Error(format!("precheck: {e}")),
                };
            }
        }
    };

    let succeeded_gs: i32 = summary
        .succeeded_ids
        .iter()
        .map(|id| match gs_by_id.get(id) {
            Some(gs) => *gs,
            None => {
                tracing::warn!(
                    title_id = %title_id,
                    ach_id = %id,
                    "gs_by_id lookup miss on chunked unlock success — crediting 0G"
                );
                0
            }
        })
        .sum();

    match summary.last_error {
        None => {
            // BUG HUNTER bbb-gs-cancel: `count` reflects only the ids that
            // actually landed, so gamerscore must too. The per-id loop can
            // break early (Stop/cancel/semaphore-closed) with NO error and
            // a partial `succeeded_ids`; crediting the full-eligible-set
            // `expected_gs` then inflates the displayed gamerscore relative
            // to `count`. Use the succeeded-only sum unless every eligible
            // id landed (in which case the two are equal anyway, preserving
            // the prior all-landed accounting exactly).
            let gamerscore = if summary.succeeded_ids.len() == eligible_ids.len() {
                expected_gs
            } else {
                succeeded_gs
            };
            TitleOutcome {
                title_id: title_id.clone(),
                name: game_name,
                result: TitleResult::Unlocked {
                    count: summary.succeeded_ids.len(),
                    gamerscore,
                },
            }
        }
        Some(e) => {
            let msg = e.to_string();
            // Classify "event-based routing required" robustly.
            // First try JSON parse of the HTTP body (the typed path —
            // `Error::HttpStatus { body, .. }` carries the Xbox
            // response verbatim), then fall back to substring match
            // for cases where the Error isn't HttpStatus or the body
            // is not JSON. Previously relied solely on substring
            // matching `"\"code\":49"`, which a whitespace-inclusive
            // variant (`{"code": 49}` with a space) would bypass. See
            // audit bug BBB-Unlock#8.
            let event_based = is_event_based_error(&e)
                || msg.contains("\"code\":49")
                || msg.contains("code 49")
                || msg.contains("None of the submitted achievements");
            if event_based && summary.succeeded_ids.is_empty() {
                // Event-based routing required AND nothing landed: code:49
                // = "none of the submitted achievements may be updated via
                // this path". Classify EventBasedSkipped so the frontend
                // prompts the user to configure the events bundle.
                TitleOutcome {
                    title_id: title_id.clone(),
                    name: game_name,
                    result: TitleResult::EventBasedSkipped,
                }
            } else if is_security_claims_error(&msg) && summary.succeeded_ids.is_empty() {
                // HTTP 403 code:21 AND nothing landed: the token isn't
                // authorized to write achievements for this title. Not a
                // real error — an auth-scope wall. Register it distinctly.
                TitleOutcome {
                    title_id: title_id.clone(),
                    name: game_name,
                    result: TitleResult::SkippedUnauthorized,
                }
            } else if summary.succeeded_ids.is_empty() {
                // No chunks landed → full error (old behaviour).
                TitleOutcome {
                    title_id: title_id.clone(),
                    name: game_name,
                    result: TitleResult::Error(msg),
                }
            } else {
                // PARTIAL: chunks 1..N-1 committed on Xbox, chunk N
                // failed. Credit the succeeded ids + their summed
                // gamerscore, surface the failure as `failed` count
                // + `last_error`. Without this branch the runner
                // silently under-reported up to `UNLOCK_CHUNK_SIZE
                // - 1` real unlocks per errored title.
                let failed = eligible_ids
                    .len()
                    .saturating_sub(summary.succeeded_ids.len());
                TitleOutcome {
                    title_id: title_id.clone(),
                    name: game_name,
                    result: TitleResult::Partial {
                        count: summary.succeeded_ids.len(),
                        gamerscore: succeeded_gs,
                        failed,
                        message: msg,
                    },
                }
            }
        }
    }
}

/// Robust detection of Xbox's "this achievement must be unlocked via
/// the events pipeline" error (code 49). Structured: parses the
/// HttpStatus body as JSON and matches on `code == 49` — no substring
/// fragility. Returns false for any non-HttpStatus error (network,
/// argument validation, etc.) so the caller can fall back to the
/// substring heuristic as a safety net.
///
/// Audit bug BBB-Unlock#8 — the Xbox response occasionally ships
/// `{"code": 49, "description": "..."}` with a space after the colon;
/// the old substring match on `"\"code\":49"` (no space) missed that
/// variant and silently classified it as a generic Error, robbing the
/// user of the "configure events bundle" hint.
pub(super) fn is_event_based_error(e: &Error) -> bool {
    let body = match e {
        Error::HttpStatus { body, .. } => body,
        _ => return false,
    };
    // Typed JSON parse. If the body isn't JSON, the fallback
    // substring match in the caller still catches the common shape.
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    v.get("code").and_then(serde_json::Value::as_i64) == Some(49)
}

/// Application-level retry wrapper around `XboxClient::unlock_achievement`
/// for the bit-by-bit event-based path.
///
/// The HTTP layer in `xbox::client::retry` already retries 429 / 503 up
/// to 4 times with throttle-gate cooperation, so by the time this
/// helper sees an `Err(HttpStatus { status: 429|5xx, .. })`, ~40 s of
/// server backoff has already been absorbed. For bit-by-bit specifically
/// we want to be a little more patient than the short-lived scan
/// request: one extra retry with a longer sleep captures the
/// "server-is-in-recovery" window. Two attempts total (initial +
/// retry). Non-transient errors (400, 401, 403, 404) bypass the retry
/// entirely — those are permanent for the given `(title, xuid, ach)`
/// tuple and another POST just burns quota.
///
/// Kept as a free function rather than a method on `XboxClient` so the
/// retry policy lives with the bit-by-bit runner it serves — other
/// unlockers may want different budgets.
/// Outcome of `unlock_with_transient_retry`.
///
/// Splitting cancellation off from the `Err` channel matters for the
/// bit-by-bit replay (the project's most timing-critical path): the
/// caller must NOT mark a Stop as a "failed unlock" or surface
/// `last_error = "cancelled during transient retry wait"` to the user.
/// See BBB-1 / BBB-2 in the bit-by-bit audit.
pub(super) enum UnlockAttempt {
    Success(#[allow(dead_code)] String),
    Cancelled,
    Failed(Error),
}

pub(super) async fn unlock_with_transient_retry(
    client: &XboxClient,
    title_id: &crate::types::TitleId,
    xuid: &crate::types::Xuid,
    achievement_id: &crate::types::AchievementId,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> UnlockAttempt {
    // First attempt. Most calls succeed here — this is the fast path.
    match client
        .unlock_achievement(title_id, xuid, achievement_id)
        .await
    {
        Ok(v) => return UnlockAttempt::Success(v),
        Err(e) if e.is_transient_server_error() => {
            // Fall through to the single retry below. Logged at
            // debug so operators can see the retry happening without
            // cluttering warn logs during ordinary success.
            tracing::debug!(
                title_id = %title_id.as_str(),
                ach_id = %achievement_id.as_str(),
                ?e,
                "bit-by-bit unlock hit transient server error after HTTP retries; waiting before one app-level retry",
            );
        }
        Err(e) => return UnlockAttempt::Failed(e),
    }
    // One retry after a longer sleep than the HTTP layer's 10 s cap.
    // The throttle gate will have absorbed the server-advertised
    // Retry-After already; this extra pause covers the case where the
    // server's rate-limit window resets on a 30-60 s cadence.
    //
    // BBB-2: previously a `cancel_rx.changed()` arm could complete with
    // `Err(_)` when the sender was dropped, then `*cancel_rx.borrow()`
    // returned the original `false`, and we fell through to the second
    // POST WITHOUT actually waiting — defeating the entire point of the
    // 15 s cool-off. Now the closed-channel case is treated as
    // cancellation (the run state is being torn down anyway, so there's
    // no plausible reason to keep retrying), and the only way the sleep
    // arm yields is genuine elapsed time.
    let mut sleeper = std::pin::pin!(tokio::time::sleep(Duration::from_secs(15)));
    loop {
        tokio::select! {
            biased;
            res = cancel_rx.changed() => {
                if res.is_err() {
                    return UnlockAttempt::Cancelled;
                }
                if *cancel_rx.borrow() {
                    return UnlockAttempt::Cancelled;
                }
                // Spurious wake (value flipped to false). Continue
                // waiting for the rest of the cool-off — do NOT skip it.
                continue;
            }
            () = &mut sleeper => break,
        }
    }
    if *cancel_rx.borrow() {
        return UnlockAttempt::Cancelled;
    }
    match client
        .unlock_achievement(title_id, xuid, achievement_id)
        .await
    {
        Ok(v) => UnlockAttempt::Success(v),
        Err(e) => UnlockAttempt::Failed(e),
    }
}
