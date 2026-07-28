//! Property tests for `plan::bit_by_bit::compute_schedule_for_title`.
//!
//! The scheduler is THE hottest pure-logic path in the codebase —
//! resume safety, ordering, and budget math all flow through it.
//! Unit tests cover the happy cases; these property tests hammer
//! invariants across hundreds of generated inputs so off-by-one bugs
//! that only surface with timestamp ties / user_last at exact
//! boundary / already-done entries mid-sequence get caught early.
//!
//! The function signature:
//!   fn compute_schedule_for_title(
//!       entries: &[PlanEntry],
//!       user_last_unix: Option<i64>,
//!       user_unlocked: &HashSet<String>,
//!       gap_mode: GapMode,
//!       now_unix: i64,
//!   ) -> Vec<ScheduledEntry>
//!
//! Invariants tested:
//!   * `out.len() == entries.len()` — no drops, no duplicates.
//!   * `scheduled_unix` is non-decreasing in source order (monotonic).
//!   * `already_done` set exactly matches the input unlocked set.
//!   * First entry with empty user history schedules at `now_unix`.
//!   * With `GapMode::Capped(d)`, consecutive gap never exceeds
//!     `d.as_secs()`.
//!   * Deterministic: same input → same output.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code

use std::collections::HashSet;
use std::time::Duration;

use proptest::prelude::*;
use xau_core::plan::bit_by_bit::{
    compute_schedule_for_title, compute_schedule_for_title_with_speedup, GapMode, MaxSpeedup,
};
use xau_core::plan::PlanEntry;

const DAY: i64 = 24 * 60 * 60;
// A reasonable "now" — 2023-11-14T22:13:20Z. Far enough in the past
// that generated unlock times (ISO strings below) are always before
// it, so scheduler behaviour matches the production "user already
// has some unlocks" regime.
const NOW: i64 = 1_700_000_000;

// ---------------------------------------------------------------------
//  Strategies
// ---------------------------------------------------------------------

/// Arbitrary PlanEntry with a parseable ISO timestamp. We fix the
/// title_id / ach_id shape so scheduler doesn't reject them.
fn entry_strategy() -> impl Strategy<Value = PlanEntry> {
    // Generate ISO timestamps by choosing a Unix time offset from
    // `1_600_000_000` (Sep 2020) to `1_700_000_000` (Nov 2023).
    // `time` crate isn't a dep here; build the ISO string manually.
    (
        (1_600_000_000_i64..=NOW),        // unix_ts
        (0u32..100_000),                  // ach id disambiguator
        proptest::option::of(0i32..1000), // gamerscore
    )
        .prop_map(|(unix_ts, ach_seq, gs)| {
            let iso = unix_to_iso(unix_ts);
            PlanEntry {
                title_id: "12345678".into(),
                service_config_id: String::new(),
                achievement_id: format!("ach-{ach_seq}"),
                achievement_name: String::new(),
                game_name: String::new(),
                time_unlocked: iso,
                gamerscore: gs,
            }
        })
}

/// Ordered vec of entries (chronological — matches how plan::builder
/// produces them). The scheduler contract ASSUMES source order is
/// oldest-first.
fn ordered_entries_strategy(min: usize, max: usize) -> impl Strategy<Value = Vec<PlanEntry>> {
    proptest::collection::vec(entry_strategy(), min..=max).prop_map(|mut v| {
        v.sort_by_key(|e| parse_iso(&e.time_unlocked).unwrap_or(0));
        // Disambiguate achievement_ids so no two entries look identical
        // when hashed into user_unlocked.
        for (i, e) in v.iter_mut().enumerate() {
            e.achievement_id = format!("ach-{i}");
        }
        v
    })
}

/// `Capped` / `Exact` / `Scaled` with a reasonable cap/factor range.
///
/// `Tiered` is deliberately OMITTED from this strategy even though the
/// variant exists: `compute_schedule_for_title` has a `debug_assert!`
/// that it's never called with Tiered (the classic scheduler anchors
/// on user_last, tiered on user_first — semantically different). The
/// shadow/tiered path has its own dedicated tests inside
/// `bit_by_bit.rs::tests::compute_tiered_schedule_*`. Including Tiered
/// here would trigger the debug_assert and fail the property run.
fn gap_mode_strategy() -> impl Strategy<Value = GapMode> {
    prop_oneof![
        Just(GapMode::Exact),
        (0u64..(DAY as u64 * 365)).prop_map(|c| GapMode::Capped(Duration::from_secs(c))),
        (0.01f32..=1.0).prop_map(GapMode::Scaled),
    ]
}

// ---------------------------------------------------------------------
//  Invariants
// ---------------------------------------------------------------------

proptest! {
    /// 1:1 mapping between input entries and output schedule entries.
    /// A scheduler that dropped entries would silently under-process
    /// the plan; a duplicate would double-count.
    #[test]
    fn schedule_length_matches_input(
        entries in ordered_entries_strategy(1, 20),
        mode in gap_mode_strategy(),
    ) {
        let out = compute_schedule_for_title(
            &entries, None, &HashSet::new(), mode, NOW,
        );
        prop_assert_eq!(out.len(), entries.len());
    }

    /// `scheduled_unix` is non-decreasing through the sequence.
    /// Breaking this would mean the runner fires achievements out of
    /// source order, violating the "bit-by-bit per-game chronology"
    /// promise.
    #[test]
    fn schedule_is_monotonic(
        entries in ordered_entries_strategy(2, 15),
        mode in gap_mode_strategy(),
    ) {
        let out = compute_schedule_for_title(
            &entries, None, &HashSet::new(), mode, NOW,
        );
        for pair in out.windows(2) {
            prop_assert!(
                pair[1].scheduled_unix >= pair[0].scheduled_unix,
                "non-monotonic: {} then {}",
                pair[0].scheduled_unix,
                pair[1].scheduled_unix
            );
        }
    }

    /// `already_done` ↔ membership in `user_unlocked`. No "almost
    /// done" states, no mis-classified entries.
    #[test]
    fn already_done_matches_user_unlocked_exactly(
        entries in ordered_entries_strategy(1, 10),
    ) {
        // Mark half the entries as already-done.
        let unlocked: HashSet<String> = entries
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 2 == 0)
            .map(|(_, e)| e.achievement_id.clone())
            .collect();
        let out = compute_schedule_for_title(
            &entries, None, &unlocked, GapMode::Exact, NOW,
        );
        for (entry, sched) in entries.iter().zip(out.iter()) {
            prop_assert_eq!(
                sched.already_done,
                unlocked.contains(&entry.achievement_id)
            );
        }
    }

    /// First entry of a user-empty title fires at `now_unix`.
    /// Regression-test against the "scheduler thinks user has
    /// history when they don't" class.
    #[test]
    fn first_entry_empty_user_fires_now(
        entries in ordered_entries_strategy(1, 5),
        mode in gap_mode_strategy(),
    ) {
        let out = compute_schedule_for_title(
            &entries, None, &HashSet::new(), mode, NOW,
        );
        // First entry's scheduled_unix == NOW (no gap before the
        // first, regardless of mode).
        prop_assert_eq!(out[0].scheduled_unix, NOW);
    }

    /// With `Capped(d)`, consecutive delta in the OUTPUT never
    /// exceeds `d.as_secs()`. The whole point of Capped is pacing —
    /// if the output exceeds the cap, the user would wait longer
    /// than they opted into.
    #[test]
    fn capped_output_respects_cap(
        entries in ordered_entries_strategy(2, 15),
        cap_secs in 60u64..(DAY as u64 * 30),
    ) {
        let mode = GapMode::Capped(Duration::from_secs(cap_secs));
        let out = compute_schedule_for_title(
            &entries, None, &HashSet::new(), mode, NOW,
        );
        for pair in out.windows(2) {
            // Only check non-already-done pairs — already_done
            // entries get the running_user_last timestamp, which
            // doesn't advance by the cap.
            if !pair[0].already_done && !pair[1].already_done {
                let delta = pair[1].scheduled_unix - pair[0].scheduled_unix;
                prop_assert!(
                    delta >= 0 && (delta as u64) <= cap_secs,
                    "capped at {cap_secs} but delta was {delta}"
                );
            }
        }
    }

    /// Deterministic: same inputs → same outputs. Scheduler with
    /// any internal non-determinism (HashMap iteration, time
    /// sampling, RNG) would fail this.
    #[test]
    fn schedule_is_deterministic(
        entries in ordered_entries_strategy(1, 10),
        mode in gap_mode_strategy(),
    ) {
        let out_1 = compute_schedule_for_title(
            &entries, None, &HashSet::new(), mode.clone(), NOW,
        );
        let out_2 = compute_schedule_for_title(
            &entries, None, &HashSet::new(), mode, NOW,
        );
        prop_assert_eq!(out_1, out_2);
    }

    /// User's existing `user_last_unix` anchors the schedule. If
    /// user_last is set, the first entry's `scheduled_unix` MUST be
    /// >= user_last.
    #[test]
    fn schedule_respects_user_last(
        entries in ordered_entries_strategy(1, 5),
        user_last_offset in 0i64..(DAY * 10),
        mode in gap_mode_strategy(),
    ) {
        let user_last = NOW - user_last_offset;
        let out = compute_schedule_for_title(
            &entries, Some(user_last), &HashSet::new(), mode, NOW,
        );
        prop_assert!(
            out[0].scheduled_unix >= user_last,
            "first sched {} < user_last {user_last}",
            out[0].scheduled_unix
        );
    }
}

// ---------------------------------------------------------------------
//  MaxSpeedup property tests — the holy-grail invariants
// ---------------------------------------------------------------------

fn max_speedup_strategy() -> impl Strategy<Value = MaxSpeedup> {
    prop_oneof![
        Just(MaxSpeedup::Off),
        Just(MaxSpeedup::Mild2x),
        Just(MaxSpeedup::Moderate3x),
        Just(MaxSpeedup::Fast5x),
        Just(MaxSpeedup::Extreme100x),
    ]
}

/// Numeric tier ordering for monotonicity assertions. The user's
/// "max" multiplier; higher = potentially more compression on a
/// given gap (capped per-gap).
fn tier_rank(t: MaxSpeedup) -> u8 {
    match t {
        MaxSpeedup::Off => 0,
        MaxSpeedup::Mild2x => 1,
        MaxSpeedup::Moderate3x => 2,
        MaxSpeedup::Fast5x => 3,
        MaxSpeedup::Extreme100x => 4,
    }
}

/// Generate "wide" plans that span hours-to-years so the per-gap
/// cap exercises every tier branch. Without this strategy, ordered
/// entries in `ordered_entries_strategy` cluster within ~3 years —
/// great for ordering tests but boring for speedup tests because
/// the per-gap cap is rarely past the 5h tier.
fn wide_span_entries_strategy(min: usize, max: usize) -> impl Strategy<Value = Vec<PlanEntry>> {
    proptest::collection::vec(
        (
            // Span the full Unix range we care about: 2010-2026.
            (1_262_304_000_i64..1_780_000_000_i64),
            (0u32..100_000),
            proptest::option::of(0i32..1000),
        ),
        min..=max,
    )
    .prop_map(|tuples| {
        let mut v: Vec<PlanEntry> = tuples
            .into_iter()
            .map(|(unix_ts, ach_seq, gs)| PlanEntry {
                title_id: "12345678".into(),
                service_config_id: String::new(),
                achievement_id: format!("ach-{ach_seq}"),
                achievement_name: String::new(),
                game_name: String::new(),
                time_unlocked: unix_to_iso(unix_ts),
                gamerscore: gs,
            })
            .collect();
        v.sort_by_key(|e| parse_iso(&e.time_unlocked).unwrap_or(0));
        for (i, e) in v.iter_mut().enumerate() {
            e.achievement_id = format!("ach-{i}");
        }
        v
    })
}

proptest! {
    /// **Holy-grail invariant.** No matter which `MaxSpeedup` the
    /// user picks, the per-title schedule is strictly non-decreasing
    /// on `(scheduled_unix, scheduled_subsec_ms)`. This is the
    /// "jamais pular ordem" guarantee — the runner will never fire
    /// achievement N+1 before achievement N within a title.
    ///
    /// Tested against random plans + every tier (including Off,
    /// where verbatim chronology must hold). **Includes a random
    /// `user_unlocked` subset** so that the path where
    /// `compute_schedule_for_title_with_speedup` interleaves
    /// already_done + pending entries is exercised. A regression
    /// where the +1ms monotonicity guard skipped already_done
    /// entries (`&& !already_done`) and let a residual-carrying
    /// already_done collapse onto a bumped-pending neighbour
    /// would be caught here.
    #[test]
    fn speedup_chronology_invariant(
        entries in wide_span_entries_strategy(2, 20),
        mode in gap_mode_strategy(),
        tier in max_speedup_strategy(),
        unlocked_seed in 0u64..1024,
    ) {
        // Deterministically pick a subset of entries to mark
        // already_done. Bit pattern from `unlocked_seed` masks each
        // entry's index → reproducible across proptest reruns from
        // the same seed file.
        let unlocked: HashSet<String> = entries
            .iter()
            .enumerate()
            .filter(|(i, _)| (unlocked_seed >> (*i % 10)) & 1 == 1)
            .map(|(_, e)| e.achievement_id.clone())
            .collect();
        let out = compute_schedule_for_title_with_speedup(
            &entries,
            None,
            &unlocked,
            mode,
            NOW,
            tier,
        );
        for w in out.windows(2) {
            let a = (w[0].scheduled_unix, w[0].scheduled_subsec_ms);
            let b = (w[1].scheduled_unix, w[1].scheduled_subsec_ms);
            prop_assert!(
                a <= b,
                "tier={tier:?} chronology broken: prev={a:?} next={b:?} (already_done flags: {}/{})",
                w[0].already_done as u8,
                w[1].already_done as u8,
            );
        }
    }

    /// **Speedup monotonicity in user_max.** For the same plan + same
    /// gap_mode, an `Extreme` schedule must NEVER produce a longer
    /// span than `Mild`. I.e. higher tier ≤ lower tier (compression
    /// monotonically grows with the tier).
    ///
    /// Compares the total span (last.scheduled_ms − first.scheduled_ms)
    /// across two random tiers — if t1 ≤ t2 in rank, span(t1) ≥ span(t2).
    #[test]
    fn speedup_monotonic_in_user_max(
        entries in wide_span_entries_strategy(3, 15),
        t1 in max_speedup_strategy(),
        t2 in max_speedup_strategy(),
    ) {
        let s1 = compute_schedule_for_title_with_speedup(
            &entries, None, &HashSet::new(), GapMode::Exact, NOW, t1,
        );
        let s2 = compute_schedule_for_title_with_speedup(
            &entries, None, &HashSet::new(), GapMode::Exact, NOW, t2,
        );
        let span_ms = |s: &[xau_core::plan::bit_by_bit::ScheduledEntry]| -> i64 {
            if s.len() < 2 { return 0; }
            let first = i64::from(s[0].scheduled_subsec_ms)
                + s[0].scheduled_unix.saturating_mul(1000);
            let last = i64::from(s[s.len()-1].scheduled_subsec_ms)
                + s[s.len()-1].scheduled_unix.saturating_mul(1000);
            (last - first).max(0)
        };
        let (rank_lo, rank_hi) = (tier_rank(t1).min(tier_rank(t2)), tier_rank(t1).max(tier_rank(t2)));
        if rank_lo == rank_hi {
            prop_assert_eq!(span_ms(&s1), span_ms(&s2));
        } else {
            let (sp_lo, sp_hi) = if tier_rank(t1) <= tier_rank(t2) {
                (span_ms(&s1), span_ms(&s2))
            } else {
                (span_ms(&s2), span_ms(&s1))
            };
            prop_assert!(
                sp_lo >= sp_hi,
                "rank {rank_lo} (sp={sp_lo}) MUST be >= rank {rank_hi} (sp={sp_hi}) — \
                 higher tier should not stretch the schedule"
            );
        }
    }

    /// **Sub-1h gap floor.** For any plan whose source gaps are ALL
    /// strictly below 1 hour, the schedule under any tier must equal
    /// the schedule under `Off` — the per-gap cap protects natural
    /// intra-session pacing from compression.
    ///
    /// We construct such a plan deterministically (gaps of 1-59
    /// minutes) and verify byte-identical schedules across tiers.
    #[test]
    fn speedup_floor_protects_sub_hour_gaps(
        // 2-10 entries, each 1-59 minutes after the previous.
        gaps_minutes in proptest::collection::vec(1u32..60, 1..10),
        tier in max_speedup_strategy(),
    ) {
        // Build entries from a fixed start, advancing by gaps_minutes.
        let mut t = 1_650_000_000_i64; // 2022-04-15
        let mut entries: Vec<PlanEntry> = vec![];
        entries.push(PlanEntry {
            title_id: "12345678".into(),
            service_config_id: String::new(),
            achievement_id: "ach-0".into(),
            achievement_name: String::new(),
            game_name: String::new(),
            time_unlocked: unix_to_iso(t),
            gamerscore: Some(10),
        });
        for (i, g) in gaps_minutes.iter().enumerate() {
            t += i64::from(*g) * 60;
            entries.push(PlanEntry {
                title_id: "12345678".into(),
                service_config_id: String::new(),
                achievement_id: format!("ach-{}", i + 1),
                achievement_name: String::new(),
                game_name: String::new(),
                time_unlocked: unix_to_iso(t),
                gamerscore: Some(10),
            });
        }
        let s_off = compute_schedule_for_title_with_speedup(
            &entries, None, &HashSet::new(), GapMode::Exact, NOW, MaxSpeedup::Off,
        );
        let s_tier = compute_schedule_for_title_with_speedup(
            &entries, None, &HashSet::new(), GapMode::Exact, NOW, tier,
        );
        for (a, b) in s_off.iter().zip(s_tier.iter()) {
            prop_assert_eq!(a.scheduled_unix, b.scheduled_unix,
                "tier {:?} compressed a sub-1h gap (forbidden by floor)", tier);
            prop_assert_eq!(a.scheduled_subsec_ms, b.scheduled_subsec_ms);
        }
    }

    /// **Short-completion guard.** A title whose total source span is
    /// strictly less than 2 hours must produce identical schedules
    /// across every tier — the 2h title-span guard protects naturally
    /// fast titles from being compressed.
    #[test]
    fn speedup_short_title_guard(
        // Generate a title with span <= 90 minutes (well below 2h).
        gaps_seconds in proptest::collection::vec(1i64..600, 1..15),
        tier in max_speedup_strategy(),
    ) {
        let mut t = 1_650_000_000_i64;
        let mut entries: Vec<PlanEntry> = vec![];
        entries.push(PlanEntry {
            title_id: "12345678".into(),
            service_config_id: String::new(),
            achievement_id: "ach-0".into(),
            achievement_name: String::new(),
            game_name: String::new(),
            time_unlocked: unix_to_iso(t),
            gamerscore: Some(10),
        });
        let mut total_span = 0_i64;
        for (i, g) in gaps_seconds.iter().enumerate() {
            t += g;
            total_span += g;
            entries.push(PlanEntry {
                title_id: "12345678".into(),
                service_config_id: String::new(),
                achievement_id: format!("ach-{}", i + 1),
                achievement_name: String::new(),
                game_name: String::new(),
                time_unlocked: unix_to_iso(t),
                gamerscore: Some(10),
            });
        }
        // Pre-condition: total span must be < 2 hours.
        prop_assume!(total_span < 2 * 60 * 60);
        let s_off = compute_schedule_for_title_with_speedup(
            &entries, None, &HashSet::new(), GapMode::Exact, NOW, MaxSpeedup::Off,
        );
        let s_tier = compute_schedule_for_title_with_speedup(
            &entries, None, &HashSet::new(), GapMode::Exact, NOW, tier,
        );
        for (a, b) in s_off.iter().zip(s_tier.iter()) {
            prop_assert_eq!(
                a.scheduled_unix,
                b.scheduled_unix,
                "short-title compressed at tier {:?} (total_span={}s)",
                tier,
                total_span
            );
            prop_assert_eq!(a.scheduled_subsec_ms, b.scheduled_subsec_ms);
        }
    }
}

// ---------------------------------------------------------------------
//  Helpers
// ---------------------------------------------------------------------

/// Minimal ISO-8601 formatter. We only need precision to the second
/// for scheduler tests; the built-in `time` crate isn't a dep.
fn unix_to_iso(unix_ts: i64) -> String {
    // Civil-from-days algorithm (Howard Hinnant) — matches what the
    // production parser expects.
    let days = unix_ts.div_euclid(86_400);
    let secs_in_day = unix_ts.rem_euclid(86_400);
    let hour = secs_in_day / 3_600;
    let minute = (secs_in_day % 3_600) / 60;
    let second = secs_in_day % 60;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Local parse of the ISO string. Matches `xau_core::plan::runner::parse_iso_public`
/// behaviour without depending on the public surface of runner.rs.
fn parse_iso(s: &str) -> Option<i64> {
    // Minimal parser — tests only call with strings produced by
    // `unix_to_iso` above.
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: i64 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: i64 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: i64 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let minute: i64 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let second: i64 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;

    // Hinnant inverse civil_from_days. Same formula as unix_to_iso
    // but producing the Unix timestamp.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * m + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}
