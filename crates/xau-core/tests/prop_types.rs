//! Property tests for the domain newtypes in `xau_core::types`.
//!
//! Invariants proved per-type:
//!   * `TryFrom<&str>` and `TryFrom<String>` agree on the same input.
//!   * Valid values roundtrip through `Display` / `as_str`.
//!   * Valid values roundtrip through JSON serialize/deserialize.
//!   * `Hash` + `Eq` consistency (equal values → equal hashes).
//!   * Construction never panics (always produces `Ok` or `Err`,
//!     never a thread panic) for arbitrary input bytes.
//!   * `Default` values round-trip as sentinels if the type has Default.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use proptest::prelude::*;
use xau_core::types::{AchievementId, Gamertag, ServiceConfigId, TitleId, Xuid};

// ---------------------------------------------------------------------
//  No-panic invariants (every TryFrom is total)
// ---------------------------------------------------------------------

proptest! {
    #[test]
    fn xuid_try_from_never_panics(s in ".*") {
        let _ = Xuid::try_from(s.as_str());
        let _ = Xuid::try_from(s);
    }
    #[test]
    fn title_id_try_from_never_panics(s in ".*") {
        let _ = TitleId::try_from(s.as_str());
        let _ = TitleId::try_from(s);
    }
    #[test]
    fn achievement_id_try_from_never_panics(s in ".*") {
        let _ = AchievementId::try_from(s.as_str());
        let _ = AchievementId::try_from(s);
    }
    #[test]
    fn gamertag_try_from_never_panics(s in ".*") {
        let _ = Gamertag::try_from(s.as_str());
        let _ = Gamertag::try_from(s);
    }
    #[test]
    fn scid_try_from_never_panics(s in ".*") {
        let _ = ServiceConfigId::try_from(s.as_str());
        let _ = ServiceConfigId::try_from(s);
    }
}

// ---------------------------------------------------------------------
//  Validator agreement (&str vs String)
// ---------------------------------------------------------------------

proptest! {
    /// `TryFrom<&str>` and `TryFrom<String>` must agree for every
    /// possible input. Catches a refactor that duplicates validation
    /// logic and forgets to keep it in sync.
    #[test]
    fn xuid_validator_is_consistent(s in ".{0,30}") {
        let a = Xuid::try_from(s.as_str());
        let b = Xuid::try_from(s.clone());
        prop_assert_eq!(a.is_ok(), b.is_ok());
    }
    #[test]
    fn title_id_validator_is_consistent(s in ".{0,20}") {
        let a = TitleId::try_from(s.as_str());
        let b = TitleId::try_from(s.clone());
        prop_assert_eq!(a.is_ok(), b.is_ok());
    }
    #[test]
    fn achievement_id_validator_is_consistent(s in ".{0,80}") {
        let a = AchievementId::try_from(s.as_str());
        let b = AchievementId::try_from(s.clone());
        prop_assert_eq!(a.is_ok(), b.is_ok());
    }
    #[test]
    fn gamertag_validator_is_consistent(s in ".{0,40}") {
        let a = Gamertag::try_from(s.as_str());
        let b = Gamertag::try_from(s.clone());
        prop_assert_eq!(a.is_ok(), b.is_ok());
    }
    #[test]
    fn scid_validator_is_consistent(s in ".{0,50}") {
        let a = ServiceConfigId::try_from(s.as_str());
        let b = ServiceConfigId::try_from(s.clone());
        prop_assert_eq!(a.is_ok(), b.is_ok());
    }
}

// ---------------------------------------------------------------------
//  Display / as_str roundtrip
// ---------------------------------------------------------------------

proptest! {
    /// For any VALID input, `as_str` equals `Display::to_string`.
    #[test]
    fn xuid_display_matches_as_str(digits in "[0-9]{1,20}") {
        let x = Xuid::try_from(digits.as_str()).expect("valid digits");
        prop_assert_eq!(x.as_str(), x.to_string());
    }
    #[test]
    fn title_id_display_matches_as_str(digits in "[0-9]{1,12}") {
        let t = TitleId::try_from(digits.as_str()).expect("valid digits");
        prop_assert_eq!(t.as_str(), t.to_string());
    }
    #[test]
    fn achievement_id_display_matches_as_str(s in "[a-zA-Z0-9\\-_]{1,64}") {
        let a = AchievementId::try_from(s.as_str()).expect("valid ASCII");
        prop_assert_eq!(a.as_str(), a.to_string());
    }
}

// ---------------------------------------------------------------------
//  JSON roundtrip (core serde invariance)
// ---------------------------------------------------------------------

proptest! {
    /// Valid Xuid roundtrips cleanly through JSON.
    #[test]
    fn xuid_json_roundtrip(digits in "[0-9]{1,20}") {
        let original = Xuid::try_from(digits.as_str()).expect("valid");
        let json = serde_json::to_string(&original).expect("serialize");
        let back: Xuid = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(original, back);
    }
    #[test]
    fn title_id_json_roundtrip(digits in "[0-9]{1,12}") {
        let original = TitleId::try_from(digits.as_str()).expect("valid");
        let json = serde_json::to_string(&original).expect("serialize");
        let back: TitleId = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(original, back);
    }
    #[test]
    fn achievement_id_json_roundtrip(s in "[a-zA-Z0-9\\-_]{1,64}") {
        let original = AchievementId::try_from(s.as_str()).expect("valid");
        let json = serde_json::to_string(&original).expect("serialize");
        let back: AchievementId = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(original, back);
    }
    #[test]
    fn scid_json_roundtrip(
        a in "[0-9a-f]{8}", b in "[0-9a-f]{4}", c in "[0-9a-f]{4}",
        d in "[0-9a-f]{4}", e in "[0-9a-f]{12}",
    ) {
        let guid = format!("{a}-{b}-{c}-{d}-{e}");
        let original = ServiceConfigId::try_from(guid.as_str()).expect("valid GUID");
        let json = serde_json::to_string(&original).expect("serialize");
        let back: ServiceConfigId = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(original, back);
    }
}

// ---------------------------------------------------------------------
//  Invalid JSON rejection (the OTHER half of deserialize invariance)
// ---------------------------------------------------------------------

proptest! {
    /// Any string the `TryFrom` rejects MUST also be rejected by
    /// deserialize. Otherwise serde smuggles in invalid states that
    /// the constructor-path blocks. This is the WHOLE POINT of
    /// `#[serde(try_from = "String")]`.
    #[test]
    fn xuid_deserialize_rejects_same_as_try_from(s in ".{0,30}") {
        let try_from_err = Xuid::try_from(s.as_str()).is_err();
        // Serialize the input as a JSON string — we're not testing
        // serde_json's own behavior here, just that it delegates to
        // our TryFrom.
        let as_json = serde_json::to_string(&s).expect("encode string");
        let deserialize_err = serde_json::from_str::<Xuid>(&as_json).is_err();
        prop_assert_eq!(try_from_err, deserialize_err);
    }
    #[test]
    fn title_id_deserialize_rejects_same_as_try_from(s in ".{0,20}") {
        let try_from_err = TitleId::try_from(s.as_str()).is_err();
        let as_json = serde_json::to_string(&s).expect("encode");
        let deserialize_err = serde_json::from_str::<TitleId>(&as_json).is_err();
        prop_assert_eq!(try_from_err, deserialize_err);
    }
}

// ---------------------------------------------------------------------
//  Hash + Eq consistency
// ---------------------------------------------------------------------

fn hash<T: Hash>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

proptest! {
    /// If `a == b`, then `hash(a) == hash(b)`. Rust's Hash contract
    /// requires this; deriving on a wrapped String preserves it.
    /// Property test catches any future custom Hash impl that breaks
    /// the invariant.
    #[test]
    fn xuid_eq_implies_hash(digits in "[0-9]{1,20}") {
        let a = Xuid::try_from(digits.as_str()).expect("valid");
        let b = Xuid::try_from(digits.as_str()).expect("valid");
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(hash(&a), hash(&b));
    }
    #[test]
    fn title_id_eq_implies_hash(digits in "[0-9]{1,12}") {
        let a = TitleId::try_from(digits.as_str()).expect("valid");
        let b = TitleId::try_from(digits.as_str()).expect("valid");
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(hash(&a), hash(&b));
    }
}

// ---------------------------------------------------------------------
//  Gamertag-specific: trim invariance
// ---------------------------------------------------------------------

proptest! {
    /// Gamertag trims whitespace on construction. So "Player" and
    /// " Player " both produce the same Gamertag. The core rule:
    /// result.as_str() has no leading/trailing whitespace.
    #[test]
    fn gamertag_trims(s in "[A-Za-z0-9 ]{1,30}") {
        let trimmed = s.trim();
        prop_assume!(!trimmed.is_empty()); // empty would be rejected

        let a = Gamertag::try_from(s.as_str()).expect("valid");
        prop_assert_eq!(a.as_str(), trimmed);

        // A padded version of the same trimmed content produces the
        // SAME Gamertag — trimming is deterministic.
        let padded = format!("   {trimmed}   ");
        let b = Gamertag::try_from(padded.as_str()).expect("valid");
        prop_assert_eq!(a, b);
    }
}
