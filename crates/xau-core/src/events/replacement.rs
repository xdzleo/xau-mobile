//! Replacement rules carried in `Data.json`.
//!
//! Each achievement entry has a bag of rules keyed by arbitrary names
//! (`EventReplacement`, `DataReplacement`, `Replacement1`, …). Every rule
//! has a `ReplacementType` discriminator + a `Target` string that is
//! substituted inside the title's JSON template.
//!
//! The "StupidFuckingLDAPTimestamp" name is copied verbatim from the
//! upstream XAU — it's the Windows `FILETIME` the Xbox client emits for
//! session start, recorded as a decimal integer in the event body.

use std::time::{SystemTime, UNIX_EPOCH};

use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use serde_json::Value;

use crate::error::Result;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "ReplacementType")]
pub enum Replacement {
    /// Literal find-and-replace. `Replacement` is typed loosely (number
    /// or string) because Data.json mixes them — we stringify at apply
    /// time.
    Replace {
        #[serde(rename = "Target")]
        target: String,
        #[serde(rename = "Replacement")]
        replacement: Value,
    },
    /// Random integer in `[Min, Max)`.
    RangeInt {
        #[serde(rename = "Target")]
        target: String,
        #[serde(rename = "Min")]
        min: i64,
        #[serde(rename = "Max")]
        max: i64,
    },
    /// Random float in `[Min, Max)`.
    RangeFloat {
        #[serde(rename = "Target")]
        target: String,
        #[serde(rename = "Min")]
        min: f64,
        #[serde(rename = "Max")]
        max: f64,
    },
    /// Windows `FILETIME` of "now" as a decimal integer. 100-ns ticks
    /// since 1601-01-01 UTC.
    #[serde(rename = "StupidFuckingLDAPTimestamp")]
    LdapTimestamp {
        #[serde(rename = "Target")]
        target: String,
    },
}

impl Replacement {
    /// Mutate `body` in place, replacing every occurrence of this rule's
    /// target with its computed replacement.
    pub fn apply(&self, body: &mut String) -> Result<()> {
        match self {
            Self::Replace {
                target,
                replacement,
            } => {
                let replacement_str = match replacement {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    Value::Null => "null".to_string(),
                    // `REPLACEDATA` substitutes whole JSON subtrees.
                    // Serialise compactly so the result stays valid JSON.
                    other => serde_json::to_string(other)?,
                };
                replace_all_in_place(body, target, &replacement_str);
            }
            Self::RangeInt { target, min, max } => {
                let lo = *min;
                let hi = (*max).max(lo.saturating_add(1));
                let v = random_int_in_range(lo, hi);
                replace_all_in_place(body, target, &v.to_string());
            }
            Self::RangeFloat { target, min, max } => {
                let lo = *min;
                let hi = if *max > lo { *max } else { lo + 1.0 };
                let v = random_float_in_range(lo, hi);
                replace_all_in_place(body, target, &format!("{v}"));
            }
            Self::LdapTimestamp { target } => {
                let ft = windows_filetime_now();
                replace_all_in_place(body, target, &ft.to_string());
            }
        }
        Ok(())
    }
}

fn replace_all_in_place(body: &mut String, needle: &str, replacement: &str) {
    if body.contains(needle) {
        *body = body.replace(needle, replacement);
    }
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
fn random_int_in_range(lo: i64, hi: i64) -> i64 {
    // Both values are bounded by Data.json-authored constants, and hi is
    // guaranteed > lo by the caller; the `as u64` is always well-defined
    // for a positive difference.
    let span = (hi - lo) as u64;
    if span == 0 {
        return lo;
    }
    let mut buf = [0u8; 8];
    OsRng.fill_bytes(&mut buf);
    let r = u64::from_le_bytes(buf);
    let offset = (r % span) as i64;
    lo + offset
}

fn random_float_in_range(lo: f64, hi: f64) -> f64 {
    let mut buf = [0u8; 8];
    OsRng.fill_bytes(&mut buf);
    let u = u64::from_le_bytes(buf);
    #[allow(clippy::cast_precision_loss)]
    let frac = (u as f64) / (u64::MAX as f64);
    lo + frac * (hi - lo)
}

/// Windows `FILETIME`-style 100-ns ticks since 1601-01-01 UTC.
/// Unix epoch = FILETIME epoch + 11,644,473,600 seconds.
fn windows_filetime_now() -> u64 {
    const TICKS_BETWEEN_EPOCHS: u64 = 11_644_473_600 * 10_000_000;
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let unix_ticks: u128 =
        u128::from(unix.as_secs()) * 10_000_000 + u128::from(unix.subsec_nanos() / 100);
    #[allow(clippy::cast_possible_truncation)]
    let total = unix_ticks + u128::from(TICKS_BETWEEN_EPOCHS);
    total as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_literal_string() {
        let rule: Replacement = serde_json::from_str(
            r#"{"ReplacementType":"Replace","Target":"REPLACEEVENT","Replacement":"Ach"}"#,
        )
        .unwrap();
        let mut body = String::from("name=REPLACEEVENT,other=REPLACEEVENT");
        rule.apply(&mut body).unwrap();
        assert_eq!(body, "name=Ach,other=Ach");
    }

    #[test]
    fn replace_numeric_replacement() {
        let rule: Replacement = serde_json::from_str(
            r#"{"ReplacementType":"Replace","Target":"REPLACECRITERIA1","Replacement":42}"#,
        )
        .unwrap();
        let mut body = String::from("count=REPLACECRITERIA1");
        rule.apply(&mut body).unwrap();
        assert_eq!(body, "count=42");
    }

    #[test]
    fn range_int_lands_inside_span() {
        let rule: Replacement =
            serde_json::from_str(r#"{"ReplacementType":"RangeInt","Target":"X","Min":5,"Max":10}"#)
                .unwrap();
        let mut body = String::from("X");
        rule.apply(&mut body).unwrap();
        let n: i64 = body.parse().unwrap();
        assert!((5..10).contains(&n));
    }

    #[test]
    fn ldap_timestamp_is_recent() {
        let rule: Replacement = serde_json::from_str(
            r#"{"ReplacementType":"StupidFuckingLDAPTimestamp","Target":"T"}"#,
        )
        .unwrap();
        let mut body = String::from("T");
        rule.apply(&mut body).unwrap();
        let ft: u64 = body.parse().unwrap();
        // Somewhere after 2020-01-01 — sanity bound only.
        assert!(ft > 132_000_000_000_000_000);
    }
}
