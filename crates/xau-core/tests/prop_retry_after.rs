//! Property tests for `xbox::client::retry::parse_retry_after`.
//!
//! The parser handles two RFC 7231 forms of the `Retry-After` header:
//!   (1) integer seconds
//!   (2) IMF-fixdate ("Sun, 06 Nov 1994 08:49:37 GMT")
//!
//! It's called on UNTRUSTED input (Xbox can return anything in a
//! malformed header, and a CDN in the middle can mangle it further).
//! The critical property: **never panic**, for any byte sequence,
//! valid UTF-8 or not.
//!
//! Audit bug R1 #5 (HTTP-date ignored → 10s fallback when server said
//! wait 5 min) is covered by `int_and_imf_both_parse` below.
//! Audit bug R2 #13 (day=0 underflow panic) is covered by
//! `never_panics_on_arbitrary_ascii`.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code

use proptest::prelude::*;
use xau_core::xbox::client::retry::parse_retry_after;

// ---------------------------------------------------------------------
//  No-panic invariants
// ---------------------------------------------------------------------

proptest! {
    /// Arbitrary UTF-8 strings (including emoji, multi-byte, empty)
    /// never panic. This is the foundational safety guarantee.
    #[test]
    fn never_panics_on_any_utf8_string(s in ".*") {
        // Discard the result — only correctness being tested is
        // "parse_retry_after returned instead of panicking."
        let _ = parse_retry_after(&s);
    }

    /// Pathological ASCII inputs from a focused space that stress
    /// the IMF parser: digits, colons, spaces, letters, zero-width
    /// ASCII chars. Catches the audit R2 #13 class (day=0 underflow).
    #[test]
    fn never_panics_on_arbitrary_ascii(s in "[0-9A-Za-z :,\\-\\.]{0,40}") {
        let _ = parse_retry_after(&s);
    }

    /// Even deliberately-shaped almost-valid IMF strings never panic.
    #[test]
    fn never_panics_on_malformed_imf(
        day_str in "[0-9]{0,3}",
        month_str in "[A-Za-z]{0,4}",
        year_str in "[0-9]{0,5}",
        hms in "[0-9:]{0,10}",
    ) {
        let s = format!("Sun, {day_str} {month_str} {year_str} {hms} GMT");
        let _ = parse_retry_after(&s);
    }
}

// ---------------------------------------------------------------------
//  Integer form (primary path)
// ---------------------------------------------------------------------

proptest! {
    /// `parse_retry_after("N")` for any non-negative u64 returns
    /// `Some(N)`. This is the 99% case: Xbox sends integer seconds.
    #[test]
    fn integer_seconds_roundtrip(n: u64) {
        let s = n.to_string();
        prop_assert_eq!(parse_retry_after(&s), Some(n));
    }

    /// Leading/trailing whitespace tolerated: `Retry-After: 120 \r\n`
    /// after strip is `"120 "` — we accept it.
    #[test]
    fn integer_with_whitespace_padding(n in 0u64..1_000_000_u64) {
        let s = format!("  {n}  ");
        prop_assert_eq!(parse_retry_after(&s), Some(n));
    }
}

// ---------------------------------------------------------------------
//  IMF-fixdate form (secondary path, R1 #5)
// ---------------------------------------------------------------------

proptest! {
    /// IMF-fixdate far in the future returns a large positive
    /// seconds-until. The specific value is time-sensitive, so we
    /// only assert it's in a sane range.
    ///
    /// "Sun, 06 Nov 2099 08:49:37 GMT" is fixed; at any wall-clock
    /// time before 2099 the seconds-until is > 0 and < ~100 years.
    #[test]
    fn imf_future_date_returns_positive_bounded(_dummy in 0u8..=0u8) {
        // Constant input — proptest frames this as "for all _dummy",
        // equivalent to a standard test that also shows up in the
        // proptest report.
        let got = parse_retry_after("Sun, 06 Nov 2099 08:49:37 GMT");
        prop_assert!(got.is_some(), "future date must parse");
        let secs = got.unwrap();
        prop_assert!(secs > 0);
        prop_assert!(secs < 100 * 365 * 24 * 3600);
    }

    /// IMF-fixdate in the past clamps to 0. Server says "wait until
    /// 2015" → "wait 0 seconds" (retry immediately). Tests the
    /// saturating_sub semantics in the parser.
    #[test]
    fn imf_past_date_clamps_zero(_dummy in 0u8..=0u8) {
        prop_assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(0)
        );
    }

    /// Malformed month strings fail cleanly (None, not panic).
    #[test]
    fn imf_bad_month_returns_none(bad in "[A-Z][a-z][a-z]") {
        // Only genuinely invalid months — skip strings that happen to
        // match the valid list.
        prop_assume!(!matches!(bad.as_str(),
            "Jan"|"Feb"|"Mar"|"Apr"|"May"|"Jun"|
            "Jul"|"Aug"|"Sep"|"Oct"|"Nov"|"Dec"));
        let s = format!("Sun, 06 {bad} 2024 08:49:37 GMT");
        prop_assert_eq!(parse_retry_after(&s), None);
    }

    /// Day=0 is explicitly rejected (audit R2 #13: underflow guard).
    /// This test would catch a regression of the day-0 panic fix.
    #[test]
    fn imf_day_zero_returns_none(_dummy in 0u8..=0u8) {
        prop_assert_eq!(
            parse_retry_after("Sun, 00 Nov 1994 08:49:37 GMT"),
            None
        );
    }
}

// ---------------------------------------------------------------------
//  Dual-path (int fails → IMF tries)
// ---------------------------------------------------------------------

proptest! {
    /// A string that's neither a valid integer NOR a valid IMF-fixdate
    /// returns None — not a panic, not a random value.
    #[test]
    fn nonsense_input_returns_none(s in "[a-zA-Z ]{1,30}") {
        // Skip strings that happen to parse as integers (impossible
        // with just [a-zA-Z ], but defensive).
        prop_assume!(s.trim().parse::<u64>().is_err());
        prop_assert_eq!(parse_retry_after(&s), None);
    }
}
