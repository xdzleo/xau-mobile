//! Per-title schedule construction from the plan's chronology: `ScheduledEntry`, catch-up shift, gap/speedup application, tiered schedules.

use super::*;

/// Per-entry outcome of the bit-by-bit scheduler. Public so the pure
/// `estimate` module (used by the GUI preview) can reuse it AND so
/// integration tests (`tests/prop_schedule.rs`) can assert invariants
/// via `proptest`.
///
/// **Sub-second precision (BBB-Subsec):** the schedule chain is
/// computed at second granularity (`scheduled_unix`) — that's what
/// `gap_mode` operates on and what the eligibility filter uses. On
/// top of that, `scheduled_subsec_ms` carries the SOURCE entry's
/// position within its second (`time_unlocked = "...:25.737Z"` →
/// `737`). The runner's sleep helper combines both into a target
/// instant `scheduled_unix * 1000 + scheduled_subsec_ms` so a burst
/// of same-second source entries fires at their original sub-second
/// cadence (`.100`, `.500`, `.999`) instead of back-to-back at
/// network speed. Field is `0..1000` by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledEntry {
    /// Achievement identifier from the source plan (`Plan.entries[i].achievement_id`).
    pub achievement_id: String,
    /// Gamerscore (Xbox "G" points) value of this achievement. Used
    /// by the runner to credit `gamerscore_this_run` only on
    /// confirmed unlocks.
    pub gamerscore: i32,
    /// Whole-second target instant (Unix seconds) at which this
    /// entry should fire. Combined with `scheduled_subsec_ms` to
    /// produce the millisecond-precise wake-up time.
    pub scheduled_unix: i64,
    /// Sub-second offset of the source entry, in milliseconds
    /// `0..1000`. Verbatim from the plan's `time_unlocked` fraction;
    /// `gap_mode` does NOT scale this — only whole-second gaps.
    pub scheduled_subsec_ms: u16,
    pub already_done: bool,
}

impl ScheduledEntry {
    /// Millisecond-precise scheduled instant.
    #[must_use]
    fn scheduled_ms(&self) -> i64 {
        self.scheduled_unix
            .saturating_mul(1000)
            .saturating_add(i64::from(self.scheduled_subsec_ms))
    }
    fn set_scheduled_ms(&mut self, ms: i64) {
        self.scheduled_unix = ms.div_euclid(1000);
        self.scheduled_subsec_ms = u16::try_from(ms.rem_euclid(1000)).unwrap_or(0);
    }
}

/// **Deficit-shift** — the anti-burst catch-up transform
/// ([`CatchUpMode::Shift`]).
///
/// When a run resumes after an absence, many entries can have a
/// `scheduled_unix` in the past (the scheduler anchors on the user's
/// true historical unlock). Firing them as-is means every overdue
/// entry is eligible at once and they land back-to-back at rate-limiter
/// speed — gaps that were days apart collapse into ~150 ms, which is
/// both the worst fidelity loss and a bot fingerprint.
///
/// This slides the WHOLE remaining (pending) chain forward by the
/// deficit `max(0, now − earliest_pending)`, so:
/// - the earliest overdue entry lands exactly at `now` (fires this run),
/// - every subsequent inter-unlock gap is preserved **verbatim** and
///   lands in the future (fires later, paced by `time_budget` / re-runs),
/// - already-done entries are untouched (they never fire), and
/// - **wire order is unchanged** — a uniform forward shift on a
///   monotonic chain stays monotonic.
///
/// No-op when nothing is overdue (deficit ≤ 0). Pure; shared by the
/// runner and the estimator so preview == run.
pub fn apply_catch_up_shift(schedule: &mut [ScheduledEntry], now_unix: i64) {
    let now_ms = now_unix.saturating_mul(1000);
    // The chain is monotonic non-decreasing, so the first pending entry
    // is also the earliest-scheduled pending entry.
    let Some(earliest_pending_ms) = schedule
        .iter()
        .find(|e| !e.already_done)
        .map(ScheduledEntry::scheduled_ms)
    else {
        return; // nothing pending
    };
    let deficit_ms = now_ms.saturating_sub(earliest_pending_ms);
    if deficit_ms <= 0 {
        return; // earliest pending is already at/after now — no burst risk
    }
    for e in schedule.iter_mut() {
        if e.already_done {
            continue;
        }
        e.set_scheduled_ms(e.scheduled_ms().saturating_add(deficit_ms));
    }
}

/// Compute the schedule for a single title's source entries. Pure:
/// same inputs → same outputs, deterministic. `now_unix` is passed
/// in so tests can freeze time.
///
/// - `entries` is the target's ordered unlock list for one title.
/// - `user_last_unix` is the latest unlock the user has on this title
///   already (`None` if they have nothing). It feeds the chain origin
///   AFTER being clamped to `max(user_last_unix, now_unix)` — Xbox
///   stamps every POST with its server-side receipt time, so the chain
///   must never start in the past, regardless of how old the user's
///   prior history is. Source gap arithmetic (entry N+1 lands
///   `effective_gap` after entry N) is unaffected by the clamp.
/// - `user_unlocked` is the set of achievement IDs the user already
///   has. Matches are marked `already_done`.
/// - `gap_mode` decides how source gaps translate to user gaps
///   (exact / capped / scaled).
/// - `now_unix` is the "treat as now" timestamp.
///
/// Shared by the live runner (`run`) AND the pure estimator; any
/// change here flows through both automatically — that's the whole
/// point of the extraction.
///
/// # Panics
///
/// In debug builds only: panics via `debug_assert!` if called with
/// `GapMode::Tiered`. Tiered uses a different anchor (user's FIRST
/// unlock) and a different running-offset model than the classic
/// `user_last_unix`-anchored chain this function implements.
/// All in-tree callers dispatch explicitly on `GapMode::Tiered` to
/// [`compute_tiered_schedule`] (see `run()` at `bit_by_bit.rs:788`
/// and the estimator at `bit_by_bit_estimate.rs:233`); this guard
/// catches a future caller who forgot. Release builds still produce
/// a valid schedule (via `GapMode::apply()`'s Tiered branch), but
/// anchored on user_last instead of user_first — semantically not a
/// "shadow replay" even though the math is sound.
#[allow(clippy::cast_possible_wrap, clippy::if_same_then_else)]
pub fn compute_schedule_for_title(
    entries: &[PlanEntry],
    user_last_unix: Option<i64>,
    user_unlocked: &std::collections::HashSet<String>,
    gap_mode: GapMode,
    now_unix: i64,
) -> Vec<ScheduledEntry> {
    // Back-compat shim: the parameterless overload assumes
    // `MaxSpeedup::Off` (verbatim chronology, the original
    // contract). New callers should use
    // `compute_schedule_for_title_with_speedup` to opt into the
    // tiered speedup ladder.
    compute_schedule_for_title_with_speedup(
        entries,
        user_last_unix,
        user_unlocked,
        gap_mode,
        now_unix,
        MaxSpeedup::Off,
    )
}

/// Same as [`compute_schedule_for_title`] but applies the user's
/// chosen [`MaxSpeedup`] tier on top of the `gap_mode` scaling.
///
/// Speedup runs AFTER `apply_gap_mode_ms` so the two compose
/// multiplicatively: `gap_mode` reshapes the timeline, then
/// speedup compresses the result. Per-gap caps + the 2-hour
/// title-span guard mean naturally-fast pacing is never touched.
///
/// **Holy-grail invariant: the per-title chronological order of
/// returned `ScheduledEntry`s is strictly non-decreasing on
/// `(scheduled_unix, scheduled_subsec_ms)` regardless of which
/// speedup is selected.** The +1ms monotonicity guard at the end
/// of the loop runs AFTER speedup compression.
pub fn compute_schedule_for_title_with_speedup(
    entries: &[PlanEntry],
    user_last_unix: Option<i64>,
    user_unlocked: &std::collections::HashSet<String>,
    gap_mode: GapMode,
    now_unix: i64,
    max_speedup: MaxSpeedup,
) -> Vec<ScheduledEntry> {
    debug_assert!(
        !matches!(gap_mode, GapMode::Tiered(_)),
        "compute_schedule_for_title called with GapMode::Tiered — \
         use compute_tiered_schedule directly (needs user_first_unix, \
         not user_last_unix). See the dispatch at bit_by_bit.rs:788."
    );
    // Compute total source span ONCE per title so we don't re-scan
    // entries on every gap. Used by the speedup gate to skip
    // naturally-fast titles entirely (span < 2h → no speedup).
    let title_span_secs: i64 = compute_title_span_secs(entries);
    // Wave-5 audit clock-F2 (HOLY-GRAIL): the bit-by-bit invariant
    // the user repeatedly emphasised — "chronological order is the
    // santo graal, MUST never break order" — required this. The
    // previous implementation iterated `entries` in input-slice
    // order, trusting the caller to pre-sort. If a plan came in
    // with even one entry out-of-order (clock skew on the source
    // profile, manually edited JSON, an aggregate-of-N merger that
    // didn't sort), the `cur > prev` guard at line 3539 silently
    // clamped the negative gap to 0 and the monotonicity guard
    // bumped each subsequent entry by exactly +1ms — preserving the
    // WRONG order with millisecond spacing. The runner then POSTed
    // unlocks out of chronological order. Fix: stable-sort by
    // parsed source timestamp at function entry. Entries with an
    // unparseable timestamp sort to the END (we can't infer their
    // chronology, so they shouldn't disrupt the ordering of valid
    // entries by appearing earlier). Stable sort preserves input
    // order for ties.
    let mut sorted: Vec<&PlanEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| {
        let parsed = crate::plan::runner::parse_iso_subsec_public(&e.time_unlocked)
            .map(|(s, n)| s.saturating_mul(1_000_000_000).saturating_add(i64::from(n)));
        // Tuple sort key: parseable entries first (false < true),
        // then by their nanosecond timestamp.
        (parsed.is_none(), parsed.unwrap_or(i64::MAX))
    });
    let mut out = Vec::with_capacity(sorted.len());
    // BBB-Subsec: chain math in milliseconds. The previous version
    // tracked `running_user_last` in seconds and tacked on the
    // entry's source sub-second offset verbatim. That was correct
    // for same-source-second bursts but broke monotonicity when
    // `gap_mode` compressed cross-second source gaps (e.g.
    // `Capped(0)` flattening a 5s gap to 0s while the source
    // sub-second offset went 999ms→0ms). Tracking the whole chain
    // in ms AND enforcing strict monotonicity below gives us:
    //   - Verbatim sub-second cadence inside any source-second
    //     burst (the case we actually care about for forensic
    //     fidelity).
    //   - Correct ordering everywhere else, even under aggressive
    //     compression modes.
    let now_ms = now_unix.saturating_mul(1000);
    let mut prev_source_ms: Option<i64> = None;
    // Anchor the chain on the user's TRUE historical last-unlock (NOT
    // clamped to now) so wall-clock time elapsed between runs counts
    // toward each gap — the module's documented stateless-resume model
    // ("A was unlocked 24h ago; B needs 30 days; run again in 30 days
    // and it fires B"). B's absolute schedule is `A_actual + gap`, a
    // run-independent constant, so re-running later simply lets `now`
    // catch up to it. Entries whose computed time is already in the
    // past fall at/before `now`; the runner's `scheduled_unix <=
    // eligibility_horizon` check fires them this run (the confirmed-
    // unlock cap + 6.67 RPS rate limiter pace the catch-up), while
    // still-future entries defer to a later run.
    //
    // (This REPLACES the earlier `.max(now_ms)` anchor clamp, which
    // rebased every run to `now` and so could never advance past a
    // title's first achievement across runs — it defeated the whole
    // incremental-resume feature under the default `time_budget: None`.
    // A fresh title with no history still anchors at `now` via the
    // `map_or(now_ms, …)` fallback below.)
    let mut running_user_last_ms = user_last_unix.map(|s| s.saturating_mul(1000));
    // Track the previous *processed entry's* scheduled_ms separately
    // from the chain anchor (`running_user_last_ms`). The
    // monotonicity guard fires only when a NEW entry would land at
    // or before a PREVIOUS entry — landing at the initial anchor
    // (no prior entry yet) is fine and shouldn't trigger a +1ms
    // bump on the very first item in the chain.
    let mut prev_entry_scheduled_ms: Option<i64> = None;
    for entry in &sorted {
        let cur_source_subsec = crate::plan::runner::parse_iso_subsec_public(&entry.time_unlocked);
        let cur_source_ms = cur_source_subsec.map(|(s, n)| {
            s.saturating_mul(1000)
                .saturating_add(i64::from(n / 1_000_000))
        });
        let source_gap_ms = match (prev_source_ms, cur_source_ms) {
            // BUG HUNTER bbb-gap-overflow: saturating — operands are
            // saturated-ms timestamps (can pin to i64::MIN/MAX on
            // adversarial years); `cur > prev` does not bound `cur - prev`,
            // and a plain subtraction panics (debug) / wraps (release),
            // defeating the saturating guards downstream.
            (Some(prev), Some(cur)) if cur > prev => cur.saturating_sub(prev),
            _ => 0,
        };
        let gap_mode_gap_ms = apply_gap_mode_ms(&gap_mode, source_gap_ms);
        // BUG HUNTER M-bbb-max-speedup: apply user's tiered speedup
        // AFTER gap_mode shaping. Per-gap cap + 2-hour title-span
        // guard ensure we only compress legitimately-dormant gaps
        // (>=1h on titles where the target spent >=2h playing).
        // For `MaxSpeedup::Off` this is a no-op pass-through and
        // chronology is 100% faithful to the source plan.
        // Classify the cap on the SOURCE gap (so a >30d/>1y source gap
        // earns its 5x/100x tier even after Capped clamps it), divide
        // the SHAPED gap. See apply_speedup_ms docs.
        let effective_gap_ms = apply_speedup_ms(
            max_speedup,
            title_span_secs,
            source_gap_ms.saturating_div(1000),
            gap_mode_gap_ms,
        );
        let already_done = user_unlocked.contains(&entry.achievement_id);
        let proposed_ms = if already_done {
            // When user_last_unix is None (no parseable timestamp on
            // any of this title's user unlocks), fall back to
            // `now_unix` — we can't chain from unknown. The
            // already_done entry won't fire anyway; it just records
            // a placeholder so `running_user_last_ms` has something
            // to chain from for the next entry.
            running_user_last_ms.unwrap_or(now_ms)
        } else {
            // Saturating add (audit bug BBB-Timing#3): `last +
            // effective_gap` can overflow for adversarial plans
            // (ISO timestamp near i64::MAX, long chains in
            // GapMode::Exact). Wrapping would produce a large-
            // negative `scheduled_ms` that looks trivially
            // eligible (everything <= eligibility_horizon), firing
            // entries that should be far-future.
            running_user_last_ms.map_or(now_ms, |last| last.saturating_add(effective_gap_ms))
        };
        // Strict monotonicity guard: when `gap_mode` collapses a
        // cross-second source gap to zero, the proposed instant can
        // land at or before the previous entry's scheduled time.
        // Bump by 1ms to keep wire-order strict — costs one ms of
        // fidelity in exchange for the order invariant the runner
        // relies on. NOTE: only fires against `prev_entry_scheduled_ms`
        // (a previous entry actually processed in THIS call); the
        // initial chain anchor is fair game.
        let scheduled_ms = match prev_entry_scheduled_ms {
            Some(prev) if proposed_ms <= prev && !already_done => prev.saturating_add(1),
            _ => proposed_ms,
        };
        let scheduled_unix = scheduled_ms.div_euclid(1000);
        let scheduled_subsec_ms = u16::try_from(scheduled_ms.rem_euclid(1000)).unwrap_or(0);
        out.push(ScheduledEntry {
            achievement_id: entry.achievement_id.clone(),
            gamerscore: entry.gamerscore.unwrap_or(0),
            scheduled_unix,
            scheduled_subsec_ms,
            already_done,
        });
        // Always advance the anchor to the current entry's scheduled
        // time, regardless of already-done vs pending vs future. This
        // preserves the chain "entry N+1 is scaled-gap after entry N"
        // for future entries too.
        //
        // Previous behaviour (only advance when `scheduled <= now`)
        // had a monotonicity bug caught by property tests: if entry[0]
        // fires now, entry[1] scheduled future, entry[2] scheduled
        // from STALE anchor (still `now`) rather than chaining from
        // entry[1]'s time. With scaled gaps where g[1→2] < g[0→1],
        // entry[2] could end up BEFORE entry[1], violating ordering.
        // See tests/prop_schedule.rs::schedule_is_monotonic.
        running_user_last_ms = Some(scheduled_ms);
        prev_entry_scheduled_ms = Some(scheduled_ms);
        // Only advance `prev_source_ms` when we actually parsed
        // THIS entry's timestamp. Overwriting with `None` on a
        // parse failure resets the chain across the hole, so the
        // NEXT parseable entry sees `source_gap = 0` instead of
        // `next_cur - last_parseable_predecessor`. One unparseable
        // entry in the middle of a list would otherwise collapse
        // the timing for everything after it. See audit bug
        // BBB-Timing#1 / #6.
        if cur_source_ms.is_some() {
            prev_source_ms = cur_source_ms;
        }
    }
    out
}

/// `gap_mode.apply(...)` adapter that operates in **milliseconds** so
/// `compute_schedule_for_title` and `compute_tiered_schedule` can
/// chain the schedule with sub-second precision while the
/// `GapMode::apply` contract stays in seconds for backwards-
/// compatibility (the property tests in `tests/prop_gap_mode.rs`
/// continue to assert on the second-precision interface).
///
/// Sub-second behaviour:
/// - **Same-source-second bursts** (`source_gap_ms < 1000`):
///   preserved verbatim. Source bursts at `.100`, `.500`, `.999`
///   end up firing at `+100ms`, `+500ms`, `+999ms` after the chain
///   anchor — exactly the cadence-fidelity goal the user asked for.
/// - **Cross-second gaps**: the whole-second part is fed to
///   `gap_mode.apply`. The sub-second residual rides along ONLY
///   when the mode preserves the gap unchanged (i.e. `Exact`, or
///   `Capped(d)` where `d` ≥ source gap). For modes that compress
///   or scale, the residual is dropped — keeping it would create
///   non-monotonic scheduling under `Capped(0)` / aggressive
///   `Tiered` speedups where two source entries get collapsed onto
///   the same scheduled second.
pub(super) fn apply_gap_mode_ms(gap_mode: &GapMode, source_gap_ms: i64) -> i64 {
    if source_gap_ms <= 0 {
        return 0;
    }
    let source_gap_secs = source_gap_ms.div_euclid(1000);
    let source_subsec_residual = source_gap_ms.rem_euclid(1000);
    let effective_secs = gap_mode.apply(source_gap_secs);
    if effective_secs == source_gap_secs {
        // Mode left the whole-second part unchanged. Usually that means the
        // mode is the identity for this gap (`Exact`, `Scaled(1.0)`, or
        // `Capped(d)` with `d > source gap`) — preserve the sub-second
        // residual too.
        //
        // BUG HUNTER bbb-capped-residual: the floored-second equality ALSO
        // holds when a `Capped(d)` clamps a gap whose WHOLE seconds equal
        // `d` but which carries a residual (e.g. 5.5s capped at 5s →
        // apply(5)==5). There the residual must be DROPPED — keeping it
        // returns 5500ms, 500ms OVER the 5000ms cap. Detect the clamp: for
        // a cross-second gap, if a one-second-larger gap would NOT advance
        // past `source_gap_secs`, the mode is clamping here. A same-second
        // burst (`source_gap_secs == 0`) is always preserved verbatim per
        // the cadence-fidelity goal (e.g. the tested `Capped(0)` burst).
        let mode_clamps_here = source_gap_secs > 0
            && gap_mode.apply(source_gap_secs.saturating_add(1)) <= source_gap_secs;
        if mode_clamps_here {
            effective_secs.saturating_mul(1000)
        } else {
            effective_secs
                .saturating_mul(1000)
                .saturating_add(source_subsec_residual)
        }
    } else {
        // Compression / scaling: drop the sub-second residual.
        effective_secs.saturating_mul(1000)
    }
}

/// "Shadow replay" scheduler for `GapMode::Tiered`. Every entry's
/// scheduled time = `user_first_unix + Σ (scaled gaps from target
/// entry 0 up to this entry)`. Each target-side gap is classified by
/// the tier table and divided by its speedup.
///
/// Why `user_first_unix` and not `user_last_unix` (the classic
/// scheduler's anchor)?
///
/// Classic bit-by-bit schedules the NEXT entry relative to the user's
/// LAST unlock — a local chain that resets after each fired unlock.
/// Tiered mode flips this: the per-title anchor is fixed to the
/// moment the title FIRST entered the user's profile. Every
/// subsequent entry is `anchor + offset_in_target's_timeline`. This
/// gives three properties:
///
/// 1. **Resumable**: re-running the program days/weeks later reads the
///    same `user_first_unix` from Xbox → schedule is recomputed
///    identically → progress just continues.
/// 2. **Monotonic**: entries fire in target's order with target-paced
///    gaps (scaled), no "catch-up burst" after absences.
/// 3. **Forensic**: our profile timeline mirrors target's CADENCE
///    starting from our anchor moment — no impossible gaps.
///
/// If `user_first_unix` is `None` (user has no unlocks on this title
/// yet), the title is treated as "not started" — the caller's
/// promotion logic decides whether to activate it this run. When the
/// caller DOES activate it, first-pass uses `now_unix` as the anchor;
/// subsequent runs will use the actual Xbox-stamped first-unlock
/// timestamp (which will be close to `now_unix` but not identical).
///
/// **Anchor clamp**: regardless of what `user_first_unix` carries,
/// the effective anchor is `max(user_first_unix, now_unix)`. Xbox
/// stamps every POST with server-side receipt time (the user cannot
/// backdate), so a historical anchor would produce schedules in the
/// past that the runner short-circuits as past-due — turning the
/// tier-scaled chain into a semaphore-rate burst. Clamping rebases
/// the chain onto NOW while preserving every accumulated tier-scaled
/// gap verbatim.
///
/// Target entries with unparseable timestamps are skipped from the
/// gap calculation but still scheduled at the running anchor position
/// — fires at the same moment as their predecessor.
pub fn compute_tiered_schedule(
    entries: &[PlanEntry],
    user_first_unix: Option<i64>,
    user_unlocked: &std::collections::HashSet<String>,
    tiers: &[(u64, f64)],
    now_unix: i64,
) -> Vec<ScheduledEntry> {
    // Back-compat shim: implicit `MaxSpeedup::Off`. New callers
    // should use `compute_tiered_schedule_with_speedup` to opt
    // into the speedup ladder layered on top of tiered scaling.
    compute_tiered_schedule_with_speedup(
        entries,
        user_first_unix,
        user_unlocked,
        tiers,
        now_unix,
        MaxSpeedup::Off,
    )
}

/// Same as [`compute_tiered_schedule`] but applies the user's
/// chosen [`MaxSpeedup`] tier on top of the tiered scaling.
pub fn compute_tiered_schedule_with_speedup(
    entries: &[PlanEntry],
    user_first_unix: Option<i64>,
    user_unlocked: &std::collections::HashSet<String>,
    tiers: &[(u64, f64)],
    now_unix: i64,
    max_speedup: MaxSpeedup,
) -> Vec<ScheduledEntry> {
    let title_span_secs: i64 = compute_title_span_secs(entries);
    // BUG HUNTER bbb-tiered-sort (HOLY-GRAIL): mirror the defensive
    // stable-sort the classic scheduler
    // (`compute_schedule_for_title_with_speedup`) applies at entry. This
    // tiered twin previously trusted the caller's slice order — but the
    // runner feeds `tp.source` in `plan.entries` order, which is NOT
    // guaranteed chronological (loaded-from-disk plans, hand-edited JSON,
    // aggregate-of-N merges, source-profile clock skew). An out-of-order
    // entry hits the `cur > prev` guard below (contributes a 0 gap) and the
    // monotonicity guard then bumps it by +1ms — silently preserving the
    // WRONG order with millisecond spacing, exactly the failure mode the
    // classic path's clock-F2 fix calls out and the module's "chronological
    // order is NEVER broken" invariant forbids. Sort by parsed source
    // timestamp; unparseable timestamps sort to the END (we can't infer
    // their chronology). Stable sort preserves input order for ties.
    let mut sorted: Vec<&PlanEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| {
        let parsed = crate::plan::runner::parse_iso_subsec_public(&e.time_unlocked)
            .map(|(s, n)| s.saturating_mul(1_000_000_000).saturating_add(i64::from(n)));
        (parsed.is_none(), parsed.unwrap_or(i64::MAX))
    });
    // BBB-Subsec: anchor + accum tracked in milliseconds. See the
    // long comment in `compute_schedule_for_title` for why the
    // chain has to live in ms (same-source-second bursts must
    // preserve their `.100/.500/.999` cadence, but Tiered's
    // aggressive compression can collapse cross-second source gaps
    // — verbatim sub-second offsets would then violate
    // monotonicity).
    // Anchor on the user's TRUE historical FIRST unlock on this title
    // (NOT clamped to now), so the "shadow replay" is resumable across
    // runs exactly as documented: every entry sits at `user_first +
    // Σ(scaled gaps)`, a run-independent absolute time, and re-running
    // later just lets `now` catch up. Past-due entries fire this run
    // via the runner's eligibility check; future ones defer. A fresh
    // title (no unlocks yet) has no first-unlock, so it anchors at
    // `now`. See the matching (fuller) rationale on the classic
    // scheduler's anchor above — this REPLACES the earlier
    // `.max(now_unix)` clamp that rebased every run to now and starved
    // cross-run resume.
    let anchor_ms = user_first_unix.unwrap_or(now_unix).saturating_mul(1000);
    let mut out = Vec::with_capacity(entries.len());
    let mut accum_scaled_ms: i64 = 0;
    let mut prev_source_ms: Option<i64> = None;
    let mut prev_scheduled_ms: Option<i64> = None;
    let tier_speedup = |gap_secs: i64| -> f64 {
        // Pick first tier whose boundary >= gap.
        tiers
            .iter()
            .find(|(max_gap, _)| (gap_secs.max(0) as u64) <= *max_gap)
            .map_or(1.0, |(_, s)| s.max(0.000_001))
    };
    for entry in sorted {
        let cur_source_subsec = crate::plan::runner::parse_iso_subsec_public(&entry.time_unlocked);
        let cur_source_ms = cur_source_subsec.map(|(s, n)| {
            s.saturating_mul(1000)
                .saturating_add(i64::from(n / 1_000_000))
        });
        // Increment the accumulated scaled offset by THIS entry's gap
        // from the previous parseable source entry. Same speedup
        // function as before (operates on whole seconds), but now
        // applied in ms and fed the source's ms-level gap so
        // sub-second residuals survive when the tier is non-
        // compressing (1.0×).
        if let (Some(prev), Some(cur)) = (prev_source_ms, cur_source_ms) {
            if cur > prev {
                // BUG HUNTER bbb-tiered-gap-overflow: saturating (same
                // i64::MIN/MAX adversarial-timestamp hazard as the classic
                // scheduler); plain `cur - prev` panics (debug) / wraps
                // (release) and corrupts the scaled offset.
                let raw_gap_ms = cur.saturating_sub(prev);
                let speedup = tier_speedup(raw_gap_ms.div_euclid(1000));
                let scaled = (raw_gap_ms as f64) / speedup.max(0.000_001);
                // BUG HUNTER M-bbb-tiered-round-consistency: previously
                // this path used `scaled as i64` (truncation toward
                // zero) while the classic scheduler in
                // `apply_speedup_ms` uses `.round() as i64`. Identical
                // source plans + identical GapMode would diverge by up
                // to 1ms per gap depending on which scheduler ran them
                // — compounded across long chains, two consecutive
                // builds (estimator vs runner) could disagree on
                // schedule by hundreds of ms. Use `.round()` to match
                // the classic scheduler. Saturating-cast guard still
                // protects against `i64::MAX` overflow on adversarial
                // year-scale inputs.
                let scaled_i64 = if scaled > i64::MAX as f64 {
                    i64::MAX
                } else {
                    scaled.round() as i64
                };
                // BUG HUNTER M-bbb-max-speedup-tiered: layer the
                // user's tiered MaxSpeedup on top of the gap-mode
                // tier scaling. Same per-gap caps and 2-hour
                // title-span guard apply — composition with
                // GapMode::Tiered is multiplicative. Classify the cap
                // on the SOURCE gap (raw), divide the tier-scaled gap.
                let post_speedup_ms = apply_speedup_ms(
                    max_speedup,
                    title_span_secs,
                    raw_gap_ms.saturating_div(1000),
                    scaled_i64,
                );
                accum_scaled_ms = accum_scaled_ms.saturating_add(post_speedup_ms);
            }
        }
        let proposed_ms = anchor_ms.saturating_add(accum_scaled_ms);
        // Strict monotonicity guard: aggressive Tiered speedups can
        // round two consecutive entries to the same scaled offset
        // (or the second to a value < first's). Bump by 1ms to keep
        // wire-order strict — costs one ms of fidelity for the
        // ordering invariant the runner relies on.
        let scheduled_ms = match prev_scheduled_ms {
            Some(prev) if proposed_ms <= prev => prev.saturating_add(1),
            _ => proposed_ms,
        };
        let scheduled_unix = scheduled_ms.div_euclid(1000);
        let scheduled_subsec_ms = u16::try_from(scheduled_ms.rem_euclid(1000)).unwrap_or(0);
        let already_done = user_unlocked.contains(&entry.achievement_id);
        out.push(ScheduledEntry {
            achievement_id: entry.achievement_id.clone(),
            gamerscore: entry.gamerscore.unwrap_or(0),
            scheduled_unix,
            scheduled_subsec_ms,
            already_done,
        });
        prev_scheduled_ms = Some(scheduled_ms);
        if cur_source_ms.is_some() {
            prev_source_ms = cur_source_ms;
        }
    }
    out
}

// ==========================================================================
//  TESTS
// ==========================================================================
