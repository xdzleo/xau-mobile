//! Unlock-safety gate — the single source of truth for "can this
//! achievement be unlocked without leaving a fingerprint?"
//!
//! # The problem
//!
//! Xbox Live will happily let you POST an `update` for **any**
//! achievement ID that exists on a title, including achievements the
//! Microsoft server itself knows are:
//!
//! - `is_revoked: true` — explicitly pulled by the publisher/MS after
//!   ship. Almost never happens but catastrophic when it does: TA
//!   community scanners + MS's own "recent unlocks of X" feeds
//!   immediately flag the account.
//! - `time_window.end_date < now` — seasonal / event achievements
//!   whose obtainable window has closed. Gamescom 2019 Challenge
//!   still "unlocks" via the API today, but **no legitimate player
//!   on earth can earn it any more** — the account lights up as
//!   cheated on any community tracker.
//! - `time_window.start_date > now` — a pre-release event that hasn't
//!   opened yet. Unlocking early creates an "unlocked before launch"
//!   anomaly that's trivially detected.
//!
//! These three flags are **authoritative** (Microsoft server tells us
//! directly), unlike TrueAchievements' community-curated "discontinued"
//! label which covers third-party knowledge (bugged achievements,
//! dead MP servers, removed DLC) that no API exposes.
//!
//! # The guarantee
//!
//! The user asked for an **absolute never-fails** guarantee that
//! revoked + time-based achievements are never unlocked. To deliver
//! it we do **belt-and-suspenders**:
//!
//! 1. **Plan-build time** (`builder.rs`) — filter unsafe entries out
//!    of the plan. Fast because metadata is already in hand from the
//!    bulk scan.
//! 2. **Pre-unlock time** (`runner.rs`, `nuclear.rs`, `bit_by_bit.rs`)
//!    — re-fetch the title's achievement metadata and re-classify
//!    just before firing the POST. Catches:
//!    - Stale plan files (built before we added the filter, or built
//!      before MS revoked an achievement that only got revoked after).
//!    - Any caller that bypassed the plan-build filter.
//! 3. **Audit trail** — every skip writes a line to
//!    `%USERPROFILE%/Documents/XAU/unlock-safety-skipped.jsonl` with
//!    the reason, title, achievement name, and timestamp. The user
//!    can `cat` it after a run to verify the gate actually caught
//!    what it was supposed to.
//!
//! # The explicit override
//!
//! For debug/dev/contrary use, the caller can construct a
//! `SafetyPolicy::danger_allow_unsafe()` which bypasses all three
//! gates. This is opt-in via the CLI `--allow-unobtainable` flag or
//! the GUI's (hidden) override. Default config = strict.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::models::achievements::{Achievement, SlimAchievement, TimeWindow};
use crate::plan::runner::parse_iso_public;

/// Internal trait abstracting the two Xbox Live achievement models
/// (`Achievement` = full record from `get_achievements_for_title`;
/// `SlimAchievement` = parse-light record from the bulk scan). The
/// safety gate operates on either — as long as the three
/// safety-critical fields (`is_revoked`, `time_window`) are
/// accessible, classification is model-agnostic.
pub trait SafetyCheckable {
    /// Raw `is_revoked` string value, if present.
    fn safety_is_revoked(&self) -> Option<&str>;
    /// Optional time-window record.
    fn safety_time_window(&self) -> Option<&TimeWindow>;
}

impl SafetyCheckable for Achievement {
    fn safety_is_revoked(&self) -> Option<&str> {
        self.is_revoked.as_deref()
    }
    fn safety_time_window(&self) -> Option<&TimeWindow> {
        self.time_window.as_ref()
    }
}

impl SafetyCheckable for SlimAchievement {
    fn safety_is_revoked(&self) -> Option<&str> {
        self.is_revoked.as_deref()
    }
    fn safety_time_window(&self) -> Option<&TimeWindow> {
        self.time_window.as_ref()
    }
}

/// Policy for the unlock-safety gate. Default is **strict** — every
/// known dangerous condition short-circuits the unlock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyPolicy {
    /// When true (default), achievements whose Xbox Live metadata
    /// marks `is_revoked = true` are skipped.
    pub skip_revoked: bool,
    /// When true (default), achievements whose `time_window.end_date`
    /// is in the past are skipped.
    pub skip_time_window_expired: bool,
    /// When true (default), achievements whose `time_window.start_date`
    /// is in the future are skipped. Edge case — rare but catches
    /// pre-release achievement data leaks.
    pub skip_time_window_not_yet_open: bool,
}

impl Default for SafetyPolicy {
    /// Safe by default. Every known danger = skip.
    fn default() -> Self {
        Self::strict()
    }
}

impl SafetyPolicy {
    /// The safe policy. All gates active. Use this everywhere except
    /// specific debug / override contexts.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            skip_revoked: true,
            skip_time_window_expired: true,
            skip_time_window_not_yet_open: true,
        }
    }

    /// **Danger** — bypasses ALL safety checks. Named `danger_*` so
    /// every call site is easy to grep. Only exposed via the CLI
    /// `--allow-unobtainable` flag.
    #[must_use]
    pub const fn danger_allow_unsafe() -> Self {
        Self {
            skip_revoked: false,
            skip_time_window_expired: false,
            skip_time_window_not_yet_open: false,
        }
    }

    /// True if ANY gate is active. Used to short-circuit the
    /// belt-and-suspenders metadata re-fetch when the policy is a
    /// pure no-op (`danger_allow_unsafe`).
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.skip_revoked || self.skip_time_window_expired || self.skip_time_window_not_yet_open
    }
}

/// The classification outcome. `Safe` = go ahead and unlock. Anything
/// else = don't. `reason_str()` gives a short label for the audit log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum SafetyClass {
    /// Clean to unlock.
    #[default]
    Safe,
    /// `is_revoked = true` on the Xbox Live record.
    Revoked,
    /// `time_window.end_date` was in the past at check time.
    TimeWindowExpired {
        /// The parsed end-date (unix seconds), for logging.
        end_unix: i64,
    },
    /// `time_window.start_date` is still in the future.
    TimeWindowNotYetOpen {
        /// The parsed start-date (unix seconds), for logging.
        start_unix: i64,
    },
    /// Title appears in the user-curated discontinued blocklist
    /// (`discontinued_default.txt` + optional
    /// `Documents/XAU/discontinued.txt` override). The whole title
    /// is filtered upstream of the per-achievement gate — no
    /// per-achievement classification is performed for these.
    /// Carried here so the audit line in `unlock-safety-skipped.jsonl`
    /// uses the same shape as the other gates.
    Discontinued,
}

impl SafetyClass {
    #[must_use]
    pub fn is_safe(&self) -> bool {
        matches!(self, Self::Safe)
    }

    /// Short human-readable label for the audit log + UI badges.
    #[must_use]
    pub fn reason_str(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Revoked => "revoked",
            Self::TimeWindowExpired { .. } => "time_window_expired",
            Self::TimeWindowNotYetOpen { .. } => "time_window_not_yet_open",
            Self::Discontinued => "discontinued",
        }
    }
}

/// Parse the `is_revoked` field into a strict bool. The Xbox API
/// returns this as a string ("true"/"false"/"1"/"0") OR a bool
/// depending on the endpoint version; our model already accepts
/// either and stores as `Option<String>`. We treat only "true"/"1"
/// (case-insensitive) as truthy; everything else is falsey.
#[must_use]
fn is_revoked_truthy(raw: Option<&str>) -> bool {
    match raw.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("true") => true,
        Some("1") => true,
        _ => false,
    }
}

/// The pure classification function. Single source of truth. Every
/// unlock path funnels through this. Pure: same inputs → same output.
///
/// Generic over [`SafetyCheckable`] so both `Achievement` (full
/// record) and `SlimAchievement` (bulk-scan record) classify the
/// same way — there's exactly ONE place in the codebase where
/// "is this unlock safe?" is decided.
///
/// - `ach` is the Xbox Live achievement record.
/// - `policy` selects which gates are active (strict by default).
/// - `now_unix` is the "treat as now" timestamp — passed in so tests
///   can freeze time.
#[must_use]
pub fn classify_achievement<A: SafetyCheckable + ?Sized>(
    ach: &A,
    policy: SafetyPolicy,
    now_unix: i64,
) -> SafetyClass {
    // Revocation is checked first — if MS marked it revoked, nothing
    // else matters (even if the time window looks clean).
    if policy.skip_revoked && is_revoked_truthy(ach.safety_is_revoked()) {
        return SafetyClass::Revoked;
    }

    if let Some(tw) = ach.safety_time_window() {
        // End-date in the past → window closed.
        if policy.skip_time_window_expired {
            if let Some(end_unix) = parse_iso_public(&tw.end_date) {
                // Explicit `end_date = "0001-..."` is Xbox's "no end
                // date" sentinel — treat as no constraint.
                if end_unix > 0 && end_unix < now_unix {
                    return SafetyClass::TimeWindowExpired { end_unix };
                }
            }
        }
        // Start-date in the future → window hasn't opened.
        if policy.skip_time_window_not_yet_open {
            if let Some(start_unix) = parse_iso_public(&tw.start_date) {
                if start_unix > now_unix {
                    return SafetyClass::TimeWindowNotYetOpen { start_unix };
                }
            }
        }
    }

    SafetyClass::Safe
}

/// One skipped-unlock record in the audit log. Written JSON-line per
/// skip to `unlock-safety-skipped.jsonl`.
///
/// Wave-10 audit cross-cutting #10: `#[serde(default)]` makes adding
/// new fields in a future version forward-compat — older readers
/// deserialize with default values rather than hard-failing on the
/// unknown field. Matches the pattern on `Plan`, `PlanEntry`,
/// `Settings`, `AuthSession`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkipAuditEntry {
    pub iso_timestamp: String,
    pub reason: String,
    pub title_id: String,
    pub title_name: String,
    pub achievement_id: String,
    pub achievement_name: String,
    pub gamerscore: i32,
    /// Which gate fired (matches `SafetyClass::reason_str`).
    pub classification: SafetyClass,
    /// Profile xuid that the runner was acting on when this skip
    /// fired. Lets multi-profile users filter the shared
    /// `unlock-safety-skipped.jsonl` by profile (`jq 'select(.xuid
    /// == "2535...")'`). Empty string when the caller doesn't have
    /// a xuid handy (e.g. unit tests with mock data) — empty means
    /// "unknown" for filter consumers.
    #[serde(default)]
    pub xuid: String,
}

/// Path to the persistent skip-audit log. Lives next to
/// `replay-errors.jsonl` — same XAU home.
#[must_use]
pub fn skip_audit_log_path() -> Option<std::path::PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(std::path::Path::to_path_buf))
        .map(|p| p.join("XAU").join("unlock-safety-skipped.jsonl"))
}

/// Gate for the 10 MiB rotation threshold (same policy as
/// replay-errors).
const AUDIT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Mutex serialises writes — audit lines are tiny so contention is
/// negligible, and we trade it for a guarantee that concurrent
/// skippers don't interleave half-lines.
static AUDIT_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Append a skip entry to the audit log. Non-fatal on I/O errors —
/// a missing log shouldn't cascade into an unlock failure. Rotates
/// the file when it crosses 10 MiB.
pub fn append_skip_audit(entry: &SkipAuditEntry) {
    use std::io::Write;
    let Some(path) = skip_audit_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    rotate_audit_log_if_needed(&path, AUDIT_ROTATE_BYTES);
    let Ok(raw_line) = serde_json::to_string(entry) else {
        return;
    };
    // Wave-5 audit JSONL F1+F2: bound the line to a safe atomic-
    // append size and escape embedded newlines so two concurrent
    // appenders (CLI + GUI) can't tear or interleave a single
    // record. The runner's `sanitize_jsonl_line` enforces both.
    let line = crate::plan::runner::sanitize_jsonl_line_public(&raw_line);
    let _lock = AUDIT_WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    // Single-syscall write of `line + "\n"` so a process kill
    // mid-call doesn't leave a half-line on disk.
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(&line);
    buf.push('\n');
    let _ = f.write_all(buf.as_bytes());
    // Wave-5 audit JSONL F3: durability — match `save_plan` and
    // the unlock-history append. Cheap (~1-3 ms on SSD) and we
    // already paid the syscall cost.
    let _ = f.sync_data();
}

/// Cross-process-safe rotation: when the live `.jsonl` exceeds
/// `threshold_bytes`, archive it to a unique sibling.
///
/// **Why not just `.jsonl.old`?** When two xau processes (CLI + GUI,
/// or two GUI windows) both crossed the threshold simultaneously they
/// would both `fs::rename(path, ".old")` — the second clobbers the
/// first's archive, irrecoverably losing a full rotation cycle of
/// audit lines. The unique suffix `(timestamp).(pid).old` eliminates
/// the collision at the cost of a slightly busier directory listing.
///
/// Public so the runner / replay-error / unlock-history loggers
/// (`runner.rs`) can share the same rotation policy without duplicating
/// the race-fix.
pub fn rotate_audit_log_if_needed(path: &std::path::Path, threshold_bytes: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= threshold_bytes {
        return;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("audit");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id();
    let archived = path.with_file_name(format!("{stem}.{now}.{pid}.old"));
    if let Err(e) = std::fs::rename(path, &archived) {
        tracing::warn!(?e, src = %path.display(), dst = %archived.display(), "audit rotate rename failed");
    }
}

/// Pre-unlock safety gate — the belt-and-suspenders companion to
/// the plan-build filter. Used by `runner.rs`, `nuclear.rs`, and
/// `bit_by_bit.rs` right before any `unlock_title_based_achievements_chunked`
/// call. Fetches the title's current achievement metadata (uses the
/// client's cache) and returns only the IDs whose classification
/// is `Safe`. Audits every rejection.
///
/// Returns `(safe_ids, count_skipped)`. When the policy is inactive
/// (`danger_allow_unsafe`), returns the input IDs untouched and
/// skips the metadata fetch entirely.
pub async fn filter_unlock_safe(
    client: &crate::xbox::XboxClient,
    xuid: &str,
    title_id: &str,
    title_name: &str,
    candidate_ids: &[String],
    policy: SafetyPolicy,
) -> crate::error::Result<(Vec<String>, usize)> {
    if !policy.is_active() || candidate_ids.is_empty() {
        return Ok((candidate_ids.to_vec(), 0));
    }
    let meta = client.get_achievements_for_title(xuid, title_id).await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0);
    // Index by ID for O(1) lookup.
    let by_id: std::collections::HashMap<&str, &Achievement> = meta
        .achievements
        .iter()
        .map(|a| (a.id.as_str(), a))
        .collect();
    let mut safe_ids = Vec::with_capacity(candidate_ids.len());
    let mut skipped = 0usize;
    for id in candidate_ids {
        let Some(ach) = by_id.get(id.as_str()) else {
            // Unknown ID = conservatively keep it (matches prior
            // behaviour). If Xbox doesn't return it in metadata it
            // probably doesn't exist; the unlock POST will 404 and
            // be counted as an error elsewhere — which is the right
            // signal. Not our job to silently drop unknowns.
            safe_ids.push(id.clone());
            continue;
        };
        let class = classify_achievement(*ach, policy, now);
        if class.is_safe() {
            safe_ids.push(id.clone());
        } else {
            skipped += 1;
            let gs = ach
                .gamerscore
                .as_ref()
                .and_then(serde_json::Value::as_i64)
                .and_then(|i| i32::try_from(i).ok())
                .or_else(|| {
                    ach.gamerscore
                        .as_ref()
                        .and_then(serde_json::Value::as_str)
                        .and_then(|s| s.parse::<i32>().ok())
                })
                .unwrap_or(0);
            append_skip_audit(&SkipAuditEntry {
                iso_timestamp: crate::plan::builder::iso_now_utc(),
                reason: class.reason_str().to_string(),
                title_id: title_id.to_string(),
                title_name: title_name.to_string(),
                achievement_id: ach.id.clone(),
                achievement_name: ach.name.clone(),
                gamerscore: gs,
                classification: class.clone(),
                xuid: xuid.to_string(),
            });
            tracing::warn!(
                title_id,
                achievement_id = %ach.id,
                reason = class.reason_str(),
                "pre-unlock safety gate blocked an achievement (gate #2)"
            );
        }
    }
    Ok((safe_ids, skipped))
}

/// Quick helper used by the builder + the pre-unlock re-check:
/// classify every item of a slice, split into `(safe, skipped)`.
/// Safe-side keeps the original achievement (Cow-friendly via ref).
/// Skipped side is the audit-ready records.
#[must_use]
pub fn partition_by_safety<'a>(
    achievements: &'a [Achievement],
    title_id: &str,
    title_name: &str,
    policy: SafetyPolicy,
    now_unix: i64,
) -> (Vec<&'a Achievement>, Vec<SkipAuditEntry>) {
    let mut safe = Vec::with_capacity(achievements.len());
    let mut skipped = Vec::new();
    if !policy.is_active() {
        // Short-circuit: policy allows everything, don't bother
        // classifying.
        safe.extend(achievements.iter());
        return (safe, skipped);
    }
    for ach in achievements {
        let class = classify_achievement(ach, policy, now_unix);
        if class.is_safe() {
            safe.push(ach);
        } else {
            let gs = ach
                .gamerscore
                .as_ref()
                .and_then(serde_json::Value::as_i64)
                .and_then(|i| i32::try_from(i).ok())
                .or_else(|| {
                    ach.gamerscore
                        .as_ref()
                        .and_then(serde_json::Value::as_str)
                        .and_then(|s| s.parse::<i32>().ok())
                })
                .unwrap_or(0);
            skipped.push(SkipAuditEntry {
                iso_timestamp: crate::plan::builder::iso_now_utc(),
                reason: class.reason_str().to_string(),
                title_id: title_id.to_string(),
                title_name: title_name.to_string(),
                achievement_id: ach.id.clone(),
                achievement_name: ach.name.clone(),
                gamerscore: gs,
                classification: class,
                // `partition_by_safety` is a pure helper without
                // a xuid in scope (it operates on a single title's
                // `Achievement` slice). Callers that DO have a xuid
                // should overwrite the `xuid` field on returned
                // entries before passing to `append_skip_audit`.
                xuid: String::new(),
            });
        }
    }
    (safe, skipped)
}

// ==========================================================================
//  TESTS
// ==========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::achievements::{Rarity, TimeWindow};

    const NOW: i64 = 1_700_000_000; // 2023-11-14T22:13:20Z

    fn mk_ach(id: &str, name: &str) -> Achievement {
        Achievement {
            id: id.to_string(),
            name: name.to_string(),
            rarity: Some(Rarity::default()),
            ..Default::default()
        }
    }

    fn mk_time_window(start: &str, end: &str) -> TimeWindow {
        TimeWindow {
            start_date: start.to_string(),
            end_date: end.to_string(),
        }
    }

    // ---- classify_achievement ---------------------------------------

    #[test]
    fn classify_safe_when_no_flags_set() {
        let ach = mk_ach("1", "normal");
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert_eq!(c, SafetyClass::Safe);
    }

    #[test]
    fn classify_revoked_true_string_flags_revoked() {
        let mut ach = mk_ach("1", "revoked");
        ach.is_revoked = Some("true".into());
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert_eq!(c, SafetyClass::Revoked);
    }

    #[test]
    fn classify_revoked_one_flags_revoked() {
        let mut ach = mk_ach("1", "revoked");
        ach.is_revoked = Some("1".into());
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert_eq!(c, SafetyClass::Revoked);
    }

    #[test]
    fn classify_revoked_mixed_case_flags_revoked() {
        let mut ach = mk_ach("1", "revoked");
        ach.is_revoked = Some("TRUE".into());
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert_eq!(c, SafetyClass::Revoked);
    }

    #[test]
    fn classify_revoked_false_string_is_safe() {
        let mut ach = mk_ach("1", "ok");
        ach.is_revoked = Some("false".into());
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert_eq!(c, SafetyClass::Safe);
    }

    #[test]
    fn classify_revoked_empty_is_safe() {
        let mut ach = mk_ach("1", "ok");
        ach.is_revoked = Some(String::new());
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert_eq!(c, SafetyClass::Safe);
    }

    #[test]
    fn classify_revoked_none_is_safe() {
        let ach = mk_ach("1", "ok");
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert_eq!(c, SafetyClass::Safe);
    }

    #[test]
    fn classify_time_window_expired_flags_expired() {
        let mut ach = mk_ach("1", "event");
        // end_date = 2019-09-15 = ~1568505600
        ach.time_window = Some(mk_time_window(
            "2019-09-01T00:00:00Z",
            "2019-09-15T23:59:00Z",
        ));
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert!(matches!(c, SafetyClass::TimeWindowExpired { .. }));
    }

    #[test]
    fn classify_time_window_future_flags_not_yet_open() {
        let mut ach = mk_ach("1", "pre-release");
        // start_date 2040 → far future
        ach.time_window = Some(mk_time_window(
            "2040-01-01T00:00:00Z",
            "2040-12-31T00:00:00Z",
        ));
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert!(matches!(c, SafetyClass::TimeWindowNotYetOpen { .. }));
    }

    #[test]
    fn classify_time_window_active_is_safe() {
        let mut ach = mk_ach("1", "current-event");
        // now = 2023-11-14; window = 2023-01 → 2024-12 → active
        ach.time_window = Some(mk_time_window(
            "2023-01-01T00:00:00Z",
            "2024-12-31T00:00:00Z",
        ));
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert_eq!(c, SafetyClass::Safe);
    }

    #[test]
    fn classify_zero_date_sentinel_is_safe() {
        // Xbox's sentinel "0001-01-01" means "no time constraint".
        // Must not be treated as expired.
        let mut ach = mk_ach("1", "no-constraint");
        ach.time_window = Some(mk_time_window(
            "0001-01-01T00:00:00Z",
            "0001-01-01T00:00:00Z",
        ));
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        assert_eq!(c, SafetyClass::Safe);
    }

    #[test]
    fn classify_revoked_beats_time_window() {
        let mut ach = mk_ach("1", "revoked-and-active");
        ach.is_revoked = Some("true".into());
        ach.time_window = Some(mk_time_window(
            "2023-01-01T00:00:00Z",
            "2099-12-31T00:00:00Z",
        ));
        let c = classify_achievement(&ach, SafetyPolicy::strict(), NOW);
        // Revocation is checked first — it wins.
        assert_eq!(c, SafetyClass::Revoked);
    }

    // ---- policy toggles ---------------------------------------------

    #[test]
    fn policy_danger_allow_unsafe_lets_revoked_through() {
        let mut ach = mk_ach("1", "revoked");
        ach.is_revoked = Some("true".into());
        let c = classify_achievement(&ach, SafetyPolicy::danger_allow_unsafe(), NOW);
        assert_eq!(c, SafetyClass::Safe);
    }

    #[test]
    fn policy_danger_allow_unsafe_lets_expired_through() {
        let mut ach = mk_ach("1", "expired");
        ach.time_window = Some(mk_time_window(
            "2019-01-01T00:00:00Z",
            "2019-12-31T00:00:00Z",
        ));
        let c = classify_achievement(&ach, SafetyPolicy::danger_allow_unsafe(), NOW);
        assert_eq!(c, SafetyClass::Safe);
    }

    #[test]
    fn policy_custom_skip_revoked_only() {
        let policy = SafetyPolicy {
            skip_revoked: true,
            skip_time_window_expired: false,
            skip_time_window_not_yet_open: false,
        };
        let mut expired = mk_ach("1", "expired");
        expired.time_window = Some(mk_time_window(
            "2019-01-01T00:00:00Z",
            "2019-12-31T00:00:00Z",
        ));
        assert_eq!(
            classify_achievement(&expired, policy, NOW),
            SafetyClass::Safe,
            "expired should not be filtered when only revoked gate is active"
        );
        let mut revoked = mk_ach("2", "revoked");
        revoked.is_revoked = Some("true".into());
        assert_eq!(
            classify_achievement(&revoked, policy, NOW),
            SafetyClass::Revoked,
        );
    }

    #[test]
    fn policy_is_active_reports_correctly() {
        assert!(SafetyPolicy::strict().is_active());
        assert!(!SafetyPolicy::danger_allow_unsafe().is_active());
    }

    // ---- partition_by_safety ----------------------------------------

    #[test]
    fn partition_separates_safe_from_unsafe() {
        let mut a1 = mk_ach("1", "ok");
        let mut a2 = mk_ach("2", "revoked");
        a2.is_revoked = Some("true".into());
        let mut a3 = mk_ach("3", "expired");
        a3.time_window = Some(mk_time_window(
            "2019-01-01T00:00:00Z",
            "2019-12-31T00:00:00Z",
        ));
        let a4 = mk_ach("4", "also-ok");
        let mut a5 = mk_ach("5", "future");
        a5.time_window = Some(mk_time_window(
            "2099-01-01T00:00:00Z",
            "2099-12-31T00:00:00Z",
        ));
        // Edit a1 to have gamerscore so the audit payload ≠ 0.
        a1.gamerscore = Some(serde_json::json!(10));
        a2.gamerscore = Some(serde_json::json!(20));
        a3.gamerscore = Some(serde_json::json!(30));
        a5.gamerscore = Some(serde_json::json!(50));
        let all = vec![a1, a2, a3, a4, a5];
        let (safe, skipped) =
            partition_by_safety(&all, "title-x", "Title X", SafetyPolicy::strict(), NOW);
        assert_eq!(safe.len(), 2);
        assert_eq!(safe[0].id, "1");
        assert_eq!(safe[1].id, "4");
        assert_eq!(skipped.len(), 3);
        let ids: Vec<&str> = skipped.iter().map(|s| s.achievement_id.as_str()).collect();
        assert!(ids.contains(&"2"));
        assert!(ids.contains(&"3"));
        assert!(ids.contains(&"5"));
        // Gamerscore flowed through.
        let g2 = skipped.iter().find(|s| s.achievement_id == "2").unwrap();
        assert_eq!(g2.gamerscore, 20);
    }

    #[test]
    fn partition_noop_when_policy_inactive() {
        let mut a1 = mk_ach("1", "revoked");
        a1.is_revoked = Some("true".into());
        let all = vec![a1];
        let (safe, skipped) =
            partition_by_safety(&all, "t", "T", SafetyPolicy::danger_allow_unsafe(), NOW);
        assert_eq!(safe.len(), 1);
        assert_eq!(skipped.len(), 0);
    }

    // ---- is_revoked_truthy -------------------------------------------

    #[test]
    fn is_revoked_truthy_handles_all_shapes() {
        assert!(is_revoked_truthy(Some("true")));
        assert!(is_revoked_truthy(Some("TRUE")));
        assert!(is_revoked_truthy(Some("True")));
        assert!(is_revoked_truthy(Some("  true  ")));
        assert!(is_revoked_truthy(Some("1")));
        assert!(!is_revoked_truthy(Some("false")));
        assert!(!is_revoked_truthy(Some("0")));
        assert!(!is_revoked_truthy(Some("")));
        assert!(!is_revoked_truthy(Some("yes")));
        assert!(!is_revoked_truthy(None));
    }

    // ---- SafetyClass::reason_str -------------------------------------

    #[test]
    fn reason_str_values_are_stable() {
        assert_eq!(SafetyClass::Safe.reason_str(), "safe");
        assert_eq!(SafetyClass::Revoked.reason_str(), "revoked");
        assert_eq!(
            SafetyClass::TimeWindowExpired { end_unix: 0 }.reason_str(),
            "time_window_expired"
        );
        assert_eq!(
            SafetyClass::TimeWindowNotYetOpen { start_unix: 0 }.reason_str(),
            "time_window_not_yet_open"
        );
    }
}
