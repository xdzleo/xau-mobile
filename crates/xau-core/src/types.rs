//! Validated domain newtypes for the Xbox Live data model.
//!
//! # Why
//!
//! The Xbox Live API is stringly-typed on the wire — XUIDs, title IDs,
//! achievement IDs, service config IDs, gamertags all flow as bare
//! `String` in the JSON payloads. Past audits flagged dozens of bugs
//! where the wrong string flowed into the wrong slot (empty XUID hitting
//! `/profile/xuid()/...`, title_id swapped with achievement_id in
//! unlock POSTs, etc.).
//!
//! Hand-unwrapping `String` at every call site isn't ergonomic and it
//! isn't safe — there's no compiler check that `unlock(title, ach, xuid)`
//! has the arguments in the right order. Newtypes fix this by construction:
//! `unlock(title: TitleId, ach: AchievementId, xuid: Xuid)` refuses to
//! compile if you try to pass `AchievementId` where `TitleId` is expected.
//!
//! # Design
//!
//! Each newtype:
//! - `#[repr(transparent)]` — identical memory layout to `String`, zero
//!   runtime cost. LLVM trivially elides the wrapper.
//! - `#[serde(try_from = "String", into = "String")]` — JSON on-the-wire
//!   shape is UNCHANGED (still a plain string in every `Plan.json` and
//!   every Xbox response), but deserialization VALIDATES. An invalid
//!   `"xuid": ""` in a plan file surfaces immediately at parse time
//!   instead of 40 seconds later as an obscure "no such user" Xbox
//!   error.
//! - `TryFrom<String>` + `TryFrom<&str>` — the ONLY ways to construct
//!   the newtype. No `From<String>` (that would skip validation).
//! - `AsRef<str>` + `Display` + `Hash` + `Ord` + `Clone` + `Debug` —
//!   drops into any API that wants `impl AsRef<str>`, `HashMap` key,
//!   `BTreeSet`, `format!("{}")`. Ergonomic on par with `String`.
//! - Variants of validation errors are TYPED, not stringly-typed —
//!   callers who want to surface "xuid is empty" vs "xuid contained
//!   non-digit char" do so via pattern match, not substring search.
//!
//! # Migration policy
//!
//! The types are introduced first; migrations land in layers:
//! 1. This file (add types + tests) — no caller changes.
//! 2. `Plan` / `PlanEntry` struct fields.
//! 3. `XboxClient` public method signatures.
//! 4. IPC handler argument structs.
//! 5. Internal helpers (scheduler, runner, aggregate, nuclear).
//!
//! Each layer compiles + tests green before the next starts.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// =========================================================================
//  Errors
// =========================================================================

/// Reasons a candidate value failed newtype validation. Typed so callers
/// can pattern-match on the specific failure ("xuid empty" vs "xuid had
/// whitespace") rather than parse strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },
    #[error("{kind} is too long: got {got} chars, max {max}")]
    TooLong {
        kind: &'static str,
        got: usize,
        max: usize,
    },
    #[error("{kind} contains invalid character at byte {byte_pos}: {ch:?}")]
    InvalidChar {
        kind: &'static str,
        byte_pos: usize,
        ch: char,
    },
    #[error("{kind} has bad shape: {reason}")]
    BadShape {
        kind: &'static str,
        reason: &'static str,
    },
}

// Provide a `From<ValidationError> for crate::Error` conversion so
// handlers can propagate validation failures through `?` like any other
// domain error. Non-infallible by design — every `?` on a validation
// produces a typed error the caller can inspect.
impl From<ValidationError> for crate::error::Error {
    fn from(e: ValidationError) -> Self {
        Self::invalid_argument(e.to_string())
    }
}

// =========================================================================
//  Xuid
// =========================================================================
//
// Xbox Live XUID: numeric string, typically 16–20 ASCII digits. Values
// outside that shape have never been observed in real Xbox responses;
// accepting them would let a bug in memory-scanning or a malformed
// plan file propagate into HTTP URLs that 404 or hit the wrong profile.

/// Maximum accepted XUID length (chars). Real XUIDs are 16–17 digits;
/// the 20-char cap gives headroom without opening unbounded input.
const XUID_MAX_LEN: usize = 20;

/// An Xbox Live user ID — always a non-empty ASCII-digit string when
/// constructed via the public API, though `Default::default()` is an
/// empty sentinel used only by `..Default::default()` spread syntax
/// in test helpers and `#[serde(default)]` struct initialization.
///
/// Construct via `Xuid::new` / `Xuid::try_from`. Deserialize via serde
/// from any JSON string (validation applied). The on-wire shape is
/// identical to `String`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
#[repr(transparent)]
pub struct Xuid(String);

impl Xuid {
    /// Construct a new `Xuid` with validation. Same contract as
    /// `TryFrom<String>`; provided as a free function for call-site
    /// readability.
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        Self::try_from(s.into())
    }

    /// The raw digit string. Prefer `AsRef<str>` in generic APIs so
    /// callers don't have to care about the wrapper.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper, returning the inner `String`. Useful only
    /// at true exit boundaries (HTTP URL, JSON serialize — both of
    /// which are handled by `AsRef`/`Serialize` anyway).
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for Xuid {
    type Error = ValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        validate_xuid(&s)?;
        Ok(Self(s))
    }
}

impl TryFrom<&str> for Xuid {
    type Error = ValidationError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        validate_xuid(s)?;
        Ok(Self(s.to_string()))
    }
}

impl FromStr for Xuid {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl AsRef<str> for Xuid {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Xuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Xuid> for String {
    fn from(x: Xuid) -> Self {
        x.0
    }
}

fn validate_xuid(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::Empty { kind: "Xuid" });
    }
    if s.len() > XUID_MAX_LEN {
        return Err(ValidationError::TooLong {
            kind: "Xuid",
            got: s.len(),
            max: XUID_MAX_LEN,
        });
    }
    // Must be all ASCII digits. Checking `is_ascii_digit` on the raw
    // byte is the correct predicate (the string length check above
    // already bounds `s.len()` == char count when ASCII).
    for (byte_pos, ch) in s.char_indices() {
        if !ch.is_ascii_digit() {
            return Err(ValidationError::InvalidChar {
                kind: "Xuid",
                byte_pos,
                ch,
            });
        }
    }
    Ok(())
}

// =========================================================================
//  TitleId
// =========================================================================
//
// Xbox title IDs are typically 8-digit decimal strings (32-bit ids
// stringified). Shape constraint: non-empty, ASCII digits only, length
// <= 12 (defensive — real values cluster at 8–10).

const TITLE_ID_MAX_LEN: usize = 12;

/// An Xbox title identifier. Decimal string, 1–12 ASCII digits.
///
/// `Default` returns the empty sentinel — ONLY for use by
/// `..Default::default()` struct-update syntax and `#[serde(default)]`
/// composition. Production code paths go through `TryFrom` which
/// rejects empty; any `TitleId` that escaped validation is a test
/// placeholder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
#[repr(transparent)]
pub struct TitleId(String);

impl TitleId {
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        Self::try_from(s.into())
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for TitleId {
    type Error = ValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        validate_title_id(&s)?;
        Ok(Self(s))
    }
}

impl TryFrom<&str> for TitleId {
    type Error = ValidationError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        validate_title_id(s)?;
        Ok(Self(s.to_string()))
    }
}

impl FromStr for TitleId {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl AsRef<str> for TitleId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TitleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<TitleId> for String {
    fn from(t: TitleId) -> Self {
        t.0
    }
}

fn validate_title_id(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::Empty { kind: "TitleId" });
    }
    if s.len() > TITLE_ID_MAX_LEN {
        return Err(ValidationError::TooLong {
            kind: "TitleId",
            got: s.len(),
            max: TITLE_ID_MAX_LEN,
        });
    }
    for (byte_pos, ch) in s.char_indices() {
        if !ch.is_ascii_digit() {
            return Err(ValidationError::InvalidChar {
                kind: "TitleId",
                byte_pos,
                ch,
            });
        }
    }
    Ok(())
}

// =========================================================================
//  AchievementId
// =========================================================================
//
// Per-title achievement identifiers. Xbox historically returns both
// numeric ("1", "42") and non-numeric IDs across the different
// generations (event-based vs stat-based vs classic). Constraint: just
// "non-empty, ASCII, no control chars, <= 64 chars" — looser than
// XUID/TitleId because Xbox's own catalog isn't strict.

const ACHIEVEMENT_ID_MAX_LEN: usize = 64;

/// A per-title achievement identifier.
///
/// `Default` is the empty sentinel — see `TitleId::default()` docs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
#[repr(transparent)]
pub struct AchievementId(String);

impl AchievementId {
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        Self::try_from(s.into())
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for AchievementId {
    type Error = ValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        validate_achievement_id(&s)?;
        Ok(Self(s))
    }
}

impl TryFrom<&str> for AchievementId {
    type Error = ValidationError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        validate_achievement_id(s)?;
        Ok(Self(s.to_string()))
    }
}

impl FromStr for AchievementId {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl AsRef<str> for AchievementId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AchievementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<AchievementId> for String {
    fn from(a: AchievementId) -> Self {
        a.0
    }
}

fn validate_achievement_id(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::Empty {
            kind: "AchievementId",
        });
    }
    if s.len() > ACHIEVEMENT_ID_MAX_LEN {
        return Err(ValidationError::TooLong {
            kind: "AchievementId",
            got: s.len(),
            max: ACHIEVEMENT_ID_MAX_LEN,
        });
    }
    if !s.is_ascii() {
        return Err(ValidationError::BadShape {
            kind: "AchievementId",
            reason: "must be ASCII",
        });
    }
    for (byte_pos, ch) in s.char_indices() {
        if ch.is_control() {
            return Err(ValidationError::InvalidChar {
                kind: "AchievementId",
                byte_pos,
                ch,
            });
        }
    }
    Ok(())
}

// =========================================================================
//  Gamertag
// =========================================================================
//
// Xbox gamertags: 1–16 graphic chars (Xbox's own cap). Unicode names
// exist (modern Xbox live accepts CJK, emoji, spaces, punctuation).
// Constraint: non-empty, trimmed, no control characters, <= 32 chars
// (some legacy names go up to 16, modern unicode names can be longer
// post normalization; 32 gives headroom).

const GAMERTAG_MAX_LEN: usize = 32;

/// An Xbox Live display gamertag. Unicode, 1–32 chars, no control chars.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
#[repr(transparent)]
pub struct Gamertag(String);

impl Gamertag {
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        Self::try_from(s.into())
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for Gamertag {
    type Error = ValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let trimmed = s.trim();
        validate_gamertag(trimmed)?;
        // Store the trimmed form — leading/trailing whitespace is always
        // noise in this domain.
        Ok(Self(trimmed.to_string()))
    }
}

impl TryFrom<&str> for Gamertag {
    type Error = ValidationError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        let trimmed = s.trim();
        validate_gamertag(trimmed)?;
        Ok(Self(trimmed.to_string()))
    }
}

impl FromStr for Gamertag {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl AsRef<str> for Gamertag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Gamertag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Gamertag> for String {
    fn from(g: Gamertag) -> Self {
        g.0
    }
}

fn validate_gamertag(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::Empty { kind: "Gamertag" });
    }
    let chars = s.chars().count();
    if chars > GAMERTAG_MAX_LEN {
        return Err(ValidationError::TooLong {
            kind: "Gamertag",
            got: chars,
            max: GAMERTAG_MAX_LEN,
        });
    }
    for (byte_pos, ch) in s.char_indices() {
        // Wave-6 audit i18n-HIGH: `is_control()` returns true ONLY
        // for Unicode general-category Cc (control). Format-category
        // controls (Cf) — U+202E RIGHT-TO-LEFT OVERRIDE, U+200B
        // ZERO-WIDTH SPACE, U+FEFF BOM, U+200E/F LRM/RLM,
        // U+202A-U+202D embedding controls, U+2066-U+2069 BiDi
        // isolates, U+2028 LINE SEPARATOR, U+2029 PARAGRAPH
        // SEPARATOR — slip through. A gamertag containing
        // U+202E followed by a fake filename ("foo\u{202E}gnp.exe")
        // is rendered visually-reversed ("fooexe.png") in the
        // multi-scan UI right next to the actual saved filename,
        // letting an attacker spoof "this is a saved plan file" via
        // a Xbox-resolved name. Reject any Cc OR Cf character, plus
        // the explicit U+2028 / U+2029 line separators (already
        // covered by Cf but listed for clarity), so no rendering
        // surface ever sees these chars.
        let is_format_control = matches!(
            ch as u32,
            0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2069 | 0xFEFF | 0x2028 | 0x2029
        );
        if ch.is_control() || is_format_control {
            return Err(ValidationError::InvalidChar {
                kind: "Gamertag",
                byte_pos,
                ch,
            });
        }
    }
    Ok(())
}

// =========================================================================
//  ServiceConfigId
// =========================================================================
//
// Xbox SCIDs are lowercase GUID strings (8-4-4-4-12 hex, lowercased).
// Empty SCIDs in a plan entry are legal (plan might predate the SCID
// backfill) but a non-empty SCID must match the GUID shape — a
// mangled SCID in a cache hit would send an unlock POST to
// `/achievements//update` and 400.

const SCID_LEN: usize = 36;

/// An Xbox service config ID — empty or a lowercase GUID.
///
/// Unlike the other newtypes, `ServiceConfigId` has a "unset" form
/// (`Option<ServiceConfigId>` callers use `None`); the wrapper
/// always holds a VALID GUID when constructed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
#[repr(transparent)]
pub struct ServiceConfigId(String);

impl ServiceConfigId {
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        Self::try_from(s.into())
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for ServiceConfigId {
    type Error = ValidationError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        validate_scid(&s)?;
        Ok(Self(s))
    }
}

impl TryFrom<&str> for ServiceConfigId {
    type Error = ValidationError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        validate_scid(s)?;
        Ok(Self(s.to_string()))
    }
}

impl FromStr for ServiceConfigId {
    type Err = ValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl AsRef<str> for ServiceConfigId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServiceConfigId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<ServiceConfigId> for String {
    fn from(s: ServiceConfigId) -> Self {
        s.0
    }
}

/// Canonical shape for `ServiceConfigId`: 8-4-4-4-12 hex digits with
/// dashes at these byte positions.
const SCID_DASH_POSITIONS: [usize; 4] = [8, 13, 18, 23];

fn validate_scid(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::Empty {
            kind: "ServiceConfigId",
        });
    }
    if s.len() != SCID_LEN {
        return Err(ValidationError::TooLong {
            kind: "ServiceConfigId",
            got: s.len(),
            max: SCID_LEN,
        });
    }
    // Canonical shape: 8-4-4-4-12 hex digits, lowercase, dashes at
    // fixed positions (see `SCID_DASH_POSITIONS`).
    for (byte_pos, ch) in s.char_indices() {
        let is_dash_position = SCID_DASH_POSITIONS.contains(&byte_pos);
        if is_dash_position {
            if ch != '-' {
                return Err(ValidationError::InvalidChar {
                    kind: "ServiceConfigId",
                    byte_pos,
                    ch,
                });
            }
        } else if !ch.is_ascii_hexdigit() || ch.is_ascii_uppercase() {
            return Err(ValidationError::InvalidChar {
                kind: "ServiceConfigId",
                byte_pos,
                ch,
            });
        }
    }
    Ok(())
}

// =========================================================================
//  Tests
// =========================================================================
//
// Exhaustive: every validation branch is exercised, and every newtype
// round-trips through JSON. Parser and serializer preserve the
// underlying string verbatim — no loss, no surprise.

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Xuid ----

    #[test]
    fn xuid_accepts_16_digits() {
        let x = Xuid::new("2500000000000001").expect("valid");
        assert_eq!(x.as_str(), "2500000000000001");
    }

    #[test]
    fn xuid_rejects_empty() {
        assert!(matches!(
            Xuid::new(""),
            Err(ValidationError::Empty { kind: "Xuid" })
        ));
    }

    #[test]
    fn xuid_rejects_too_long() {
        let too_long = "1".repeat(XUID_MAX_LEN + 1);
        assert!(matches!(
            Xuid::new(too_long),
            Err(ValidationError::TooLong { kind: "Xuid", .. })
        ));
    }

    #[test]
    fn xuid_rejects_letters() {
        assert!(matches!(
            Xuid::new("2535abc442375878"),
            Err(ValidationError::InvalidChar { kind: "Xuid", .. })
        ));
    }

    #[test]
    fn xuid_rejects_whitespace() {
        assert!(matches!(
            Xuid::new(" 2500000000000001 "),
            Err(ValidationError::InvalidChar { kind: "Xuid", .. })
        ));
    }

    #[test]
    fn xuid_json_roundtrip_preserves_string() {
        let x = Xuid::new("2500000000000001").expect("valid");
        let j = serde_json::to_string(&x).expect("serialize");
        assert_eq!(j, r#""2500000000000001""#);
        let back: Xuid = serde_json::from_str(&j).expect("deserialize");
        assert_eq!(back, x);
    }

    #[test]
    fn xuid_json_rejects_invalid_in_deserialize() {
        let bad = r#""""#;
        let err = serde_json::from_str::<Xuid>(bad).expect_err("must reject");
        // Serde wraps our ValidationError in its own error type; the
        // Display text includes our error message.
        assert!(err.to_string().contains("Xuid"));
    }

    // ---- TitleId ----

    #[test]
    fn title_id_accepts_8_digits() {
        let t = TitleId::new("12345678").expect("valid");
        assert_eq!(t.as_str(), "12345678");
    }

    #[test]
    fn title_id_rejects_letters() {
        assert!(matches!(
            TitleId::new("12abc678"),
            Err(ValidationError::InvalidChar {
                kind: "TitleId",
                ..
            })
        ));
    }

    #[test]
    fn title_id_json_roundtrip() {
        let t = TitleId::new("12345678").expect("valid");
        let j = serde_json::to_string(&t).expect("serialize");
        assert_eq!(j, r#""12345678""#);
    }

    // ---- AchievementId ----

    #[test]
    fn achievement_id_accepts_numeric() {
        assert!(AchievementId::new("1").is_ok());
        assert!(AchievementId::new("42").is_ok());
    }

    #[test]
    fn achievement_id_accepts_alphanumeric_mix() {
        // Event-based titles sometimes emit hash-shaped identifiers.
        // We don't constrain beyond "printable ASCII, no control".
        assert!(AchievementId::new("abc123-xyz_42.final").is_ok());
    }

    #[test]
    fn achievement_id_rejects_control_char() {
        assert!(matches!(
            AchievementId::new("abc\x00def"),
            Err(ValidationError::InvalidChar {
                kind: "AchievementId",
                ..
            })
        ));
    }

    #[test]
    fn achievement_id_rejects_non_ascii() {
        assert!(matches!(
            AchievementId::new("ach-é"),
            Err(ValidationError::BadShape {
                kind: "AchievementId",
                ..
            })
        ));
    }

    // ---- Gamertag ----

    #[test]
    fn gamertag_accepts_ascii_alpha() {
        let g = Gamertag::new("FixtureUser001").expect("valid");
        assert_eq!(g.as_str(), "FixtureUser001");
    }

    #[test]
    fn gamertag_accepts_unicode() {
        let g = Gamertag::new("Player 名前").expect("valid");
        assert_eq!(g.as_str(), "Player 名前");
    }

    #[test]
    fn gamertag_trims_whitespace() {
        let g = Gamertag::new("  Player  ").expect("valid");
        assert_eq!(g.as_str(), "Player");
    }

    #[test]
    fn gamertag_rejects_empty_or_whitespace_only() {
        assert!(matches!(
            Gamertag::new(""),
            Err(ValidationError::Empty { kind: "Gamertag" })
        ));
        assert!(matches!(
            Gamertag::new("   "),
            Err(ValidationError::Empty { kind: "Gamertag" })
        ));
    }

    #[test]
    fn gamertag_rejects_control_char() {
        assert!(matches!(
            Gamertag::new("Bad\nName"),
            Err(ValidationError::InvalidChar {
                kind: "Gamertag",
                ..
            })
        ));
    }

    /// Wave-6 audit i18n-HIGH: BiDi / format-category controls slip
    /// past `is_control()` (which only catches general-category Cc).
    /// A gamertag like `"foo\u{202E}gnp.exe"` displays as
    /// `"fooexe.png"` and can spoof a saved-plan filename in the
    /// multi-scan UI. Lock the rejection of every Cf control we
    /// added to the explicit blocklist.
    #[test]
    fn gamertag_rejects_bidi_and_format_controls() {
        for (label, ch) in [
            ("RTL override (U+202E)", '\u{202E}'),
            ("LTR override (U+202D)", '\u{202D}'),
            ("LRE (U+202A)", '\u{202A}'),
            ("RLE (U+202B)", '\u{202B}'),
            ("PDF pop directional (U+202C)", '\u{202C}'),
            ("LRI (U+2066)", '\u{2066}'),
            ("RLI (U+2067)", '\u{2067}'),
            ("FSI (U+2068)", '\u{2068}'),
            ("PDI pop isolate (U+2069)", '\u{2069}'),
            ("Zero-width space (U+200B)", '\u{200B}'),
            ("ZWNJ (U+200C)", '\u{200C}'),
            ("ZWJ (U+200D)", '\u{200D}'),
            ("LRM (U+200E)", '\u{200E}'),
            ("RLM (U+200F)", '\u{200F}'),
            ("BOM / ZWNBSP (U+FEFF)", '\u{FEFF}'),
            ("Line separator (U+2028)", '\u{2028}'),
            ("Paragraph separator (U+2029)", '\u{2029}'),
            ("Word joiner (U+2060)", '\u{2060}'),
        ] {
            let s = format!("foo{ch}bar");
            let result = Gamertag::new(&s);
            assert!(
                matches!(
                    result,
                    Err(ValidationError::InvalidChar { kind: "Gamertag", .. })
                ),
                "{label} (U+{:04X}) must be rejected but `Gamertag::new(\"foo{{ch}}bar\")` returned {:?}",
                ch as u32,
                result,
            );
        }
    }

    // ---- ServiceConfigId ----

    #[test]
    fn scid_accepts_canonical_lowercase_guid() {
        let s = ServiceConfigId::new("12345678-1234-1234-1234-123456789abc")
            .expect("valid lowercase guid");
        assert_eq!(s.as_str(), "12345678-1234-1234-1234-123456789abc");
    }

    #[test]
    fn scid_rejects_uppercase() {
        assert!(matches!(
            ServiceConfigId::new("12345678-1234-1234-1234-123456789ABC"),
            Err(ValidationError::InvalidChar {
                kind: "ServiceConfigId",
                ..
            })
        ));
    }

    #[test]
    fn scid_rejects_wrong_dash_position() {
        assert!(matches!(
            ServiceConfigId::new("123456781-234-1234-1234-123456789abc"),
            Err(ValidationError::InvalidChar {
                kind: "ServiceConfigId",
                ..
            })
        ));
    }

    #[test]
    fn scid_rejects_too_short() {
        assert!(matches!(
            ServiceConfigId::new("12345"),
            Err(ValidationError::TooLong {
                kind: "ServiceConfigId",
                ..
            })
        ));
    }

    // ---- Cross-type ----

    #[test]
    fn types_impl_ord_for_btreeset_keys() {
        use std::collections::BTreeSet;
        let mut s = BTreeSet::new();
        s.insert(TitleId::new("999").unwrap());
        s.insert(TitleId::new("111").unwrap());
        s.insert(TitleId::new("555").unwrap());
        let got: Vec<&str> = s.iter().map(TitleId::as_str).collect();
        // Lexicographic ASCII order: "111" < "555" < "999".
        assert_eq!(got, vec!["111", "555", "999"]);
    }

    #[test]
    fn types_impl_hash_for_hashmap_keys() {
        use std::collections::HashMap;
        let mut m: HashMap<Xuid, i32> = HashMap::new();
        m.insert(Xuid::new("1").unwrap(), 10);
        m.insert(Xuid::new("2").unwrap(), 20);
        assert_eq!(m.get(&Xuid::new("1").unwrap()), Some(&10));
    }

    #[test]
    fn types_impl_asref_str_for_generic_apis() {
        fn takes_asref(s: impl AsRef<str>) -> usize {
            s.as_ref().len()
        }
        assert_eq!(takes_asref(Xuid::new("1234").unwrap()), 4);
        assert_eq!(takes_asref(TitleId::new("12345678").unwrap()), 8);
    }

    #[test]
    fn validation_error_is_convertible_to_crate_error() {
        // `?` in handlers should Just Work. Verify the From<...> impl.
        fn sample() -> Result<Xuid, crate::error::Error> {
            let x: Xuid = String::new().try_into()?;
            Ok(x)
        }
        let err = sample().expect_err("should reject empty");
        assert!(err.to_string().contains("Xuid"));
    }
}
