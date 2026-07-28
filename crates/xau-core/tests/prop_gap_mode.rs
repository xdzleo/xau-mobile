//! Property tests for `plan::bit_by_bit::GapMode::apply`.
//!
//! These prove invariants that MUST hold for every input, regardless
//! of the specific integer value. Unit tests pick 3-5 fixed cases;
//! `proptest` generates hundreds of random cases and shrinks failures
//! to a minimal example, catching off-by-one / overflow bugs that
//! fixed-case tests don't reach.
//!
//! The invariants tested here reflect the published `GapMode` contract:
//! `Exact` preserves, `Capped(d)` clamps to `d.as_secs()`, `Scaled(f)`
//! multiplies (with NaN/negative/out-of-range clamped to valid range).

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code

use std::time::Duration;

use proptest::prelude::*;
use xau_core::plan::bit_by_bit::GapMode;

// ---------------------------------------------------------------------
//  Exact mode
// ---------------------------------------------------------------------

proptest! {
    /// `Exact` preserves non-negative input verbatim.
    #[test]
    fn exact_preserves_non_negative(g in 0i64..(i64::MAX / 2)) {
        prop_assert_eq!(GapMode::Exact.apply(g), g);
    }

    /// `Exact` clamps negatives to 0 — a negative gap is defensive
    /// nonsense (entries should arrive in order) so we treat it as
    /// "no gap" rather than propagating a time-reversal.
    #[test]
    fn exact_clamps_negative_to_zero(g in i64::MIN..0i64) {
        prop_assert_eq!(GapMode::Exact.apply(g), 0);
    }
}

// ---------------------------------------------------------------------
//  Capped mode
// ---------------------------------------------------------------------

proptest! {
    /// `Capped(cap)` NEVER exceeds `cap.as_secs()`.
    #[test]
    fn capped_never_exceeds_cap(
        g in 0i64..(i64::MAX / 2),
        cap_secs in 0u64..(u64::MAX / 2)
    ) {
        let mode = GapMode::Capped(Duration::from_secs(cap_secs));
        let out = mode.apply(g);
        prop_assert!(out >= 0);
        // i64 cast of cap_secs is provably safe because we bounded
        // cap_secs < u64::MAX / 2 < i64::MAX.
        prop_assert!(out <= cap_secs as i64);
    }

    /// `Capped(d)` on gap <= d.as_secs() preserves the gap.
    #[test]
    fn capped_preserves_under_cap(
        g in 0i64..86_400_i64,                  // <= 1 day
        cap_secs in 86_400_u64..86_400_u64 * 30 // 1..30 days cap
    ) {
        let mode = GapMode::Capped(Duration::from_secs(cap_secs));
        // g is bounded < cap, so apply should return g unchanged.
        prop_assert_eq!(mode.apply(g), g);
    }

    /// `Capped(d)` clamps negatives to 0 just like Exact.
    #[test]
    fn capped_negative_clamps_zero(
        g in i64::MIN..0i64,
        cap_secs in 0u64..(u64::MAX / 2)
    ) {
        let mode = GapMode::Capped(Duration::from_secs(cap_secs));
        prop_assert_eq!(mode.apply(g), 0);
    }
}

// ---------------------------------------------------------------------
//  Scaled mode
// ---------------------------------------------------------------------

proptest! {
    /// `Scaled(f)` with f in [0.0, 1.0] NEVER exceeds the input.
    /// (f > 1.0 gets clamped to 1.0 by `apply`, which is equivalent to
    /// Exact.)
    #[test]
    fn scaled_never_exceeds_input(
        g in 0i64..1_000_000_000_i64,
        f in 0.0f32..=1.0
    ) {
        let out = GapMode::Scaled(f).apply(g);
        prop_assert!(out >= 0);
        prop_assert!(out <= g);
    }

    /// `Scaled(0.0)` collapses to 0. This is the "anti-pattern"
    /// documented in audit bug R1 #5 — the GUI must not send 0.0 but
    /// if it does, we still produce a valid (and intentional) output.
    #[test]
    fn scaled_zero_is_zero(g in 0i64..1_000_000_000_i64) {
        prop_assert_eq!(GapMode::Scaled(0.0).apply(g), 0);
    }

    /// `Scaled(1.0)` is equivalent to `Exact`.
    #[test]
    fn scaled_one_equals_exact(g in 0i64..1_000_000_000_i64) {
        prop_assert_eq!(GapMode::Scaled(1.0).apply(g), GapMode::Exact.apply(g));
    }

    /// Out-of-range factor gets clamped to [0, 1] — does NOT panic.
    /// Audit R1 #5 concern: `factor: f32::NAN` mustn't panic. Covered
    /// here with random f32 values including NaN/±Infinity via the
    /// proptest::num::f32::ANY strategy.
    #[test]
    fn scaled_never_panics_on_any_f32(
        g in 0i64..1_000_000_000_i64,
        f in proptest::num::f32::ANY
    ) {
        // Just assert it terminates — we don't care about the value.
        let _ = GapMode::Scaled(f).apply(g);
    }

    /// Monotonic in the input gap: larger gap in → larger (or equal)
    /// gap out. Breaking this means the scheduler could reorder
    /// entries in time.
    #[test]
    fn scaled_monotonic_in_input(
        g1 in 0i64..500_000_000_i64,
        delta in 1i64..500_000_000_i64,
        f in 0.0f32..=1.0
    ) {
        let g2 = g1 + delta;
        let mode = GapMode::Scaled(f);
        prop_assert!(mode.apply(g2) >= mode.apply(g1));
    }
}

// ---------------------------------------------------------------------
//  Cross-mode
// ---------------------------------------------------------------------

proptest! {
    /// For any `g >= 0`, result is non-negative across all modes.
    /// This is a catch-all against any future negative-result bug.
    #[test]
    fn all_modes_non_negative_for_non_negative_input(
        g in 0i64..(i64::MAX / 2),
        cap_secs in 0u64..(u64::MAX / 2),
        f in proptest::num::f32::ANY,
    ) {
        prop_assert!(GapMode::Exact.apply(g) >= 0);
        prop_assert!(GapMode::Capped(Duration::from_secs(cap_secs)).apply(g) >= 0);
        prop_assert!(GapMode::Scaled(f).apply(g) >= 0);
    }
}
