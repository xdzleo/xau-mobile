//! Gap-handling + speedup value model: `GapMode`, `MaxSpeedup`, and the per-gap/per-title speedup math.

use super::*;

/// How to map the target's real-world gap between two unlocks onto
/// the user's real-world wait. Each mode produces a different
/// fidelity/speed trade-off — none is "best"; the user picks.
#[derive(Debug, Clone)]
pub enum GapMode {
    /// Fidelity mode: the real-world gap from the target's timeline
    /// is preserved EXACTLY. If the target took 300 days between
    /// achievement A and B, you wait 300 days. Honest to the source
    /// plan but can mean weeks of elapsed time — caller must know.
    Exact,
    /// Compat mode (legacy default): clamp each gap to `Duration`
    /// so a pathological 300-day gap collapses to 30 days but the
    /// relative ordering + sub-cap gaps are preserved verbatim.
    /// This was the historical default; kept for the "don't think
    /// about it" path.
    Capped(Duration),
    /// Speedrun mode: gap × factor. `Scaled(0.1)` turns a 10-hour
    /// target span into a 1-hour user span while keeping the shape
    /// of the cadence. Useful for "replicate the FLOW but faster".
    /// Factor is bounded to `(0.0, 1.0]` by the runner — values
    /// outside that range are clamped silently (clamping-in-the-math
    /// is better than surprising error paths).
    Scaled(f32),
    /// Tiered "shadow replay" mode: different speedups apply to
    /// different gap ranges. The canonical use case is "preserve
    /// natural pacing within an active session (gaps <1h) but
    /// compress dormant periods (gaps of days/months) so cloning a
    /// target's 12-year career fits in 6-12 months of wall time".
    ///
    /// Each tier is `(max_gap_secs, speedup)`. Gaps are classified
    /// by the FIRST tier whose `max_gap_secs` they fit under.
    /// Terminal catch-all tier should have `max_gap_secs = u64::MAX`.
    ///
    /// Example PSICOTA preset:
    ///   `[(3600, 1.0), (7200, 2.0), (259200, 50.0),
    ///     (2592000, 1000.0), (7776000, 100000.0),
    ///     (31104000, 1000000.0), (u64::MAX, 10000000.0)]`
    ///
    /// Tiered mode also triggers the scheduler's "shadow anchor"
    /// semantics: per-title anchor = user's FIRST unlock on that
    /// title (not LAST). Scheduled times are `anchor + sum of
    /// scaled gaps from target_entry_0 up to this entry`.
    Tiered(Vec<(u64, f64)>),
}

impl GapMode {
    /// Apply the mode to a single source gap. Input/output in
    /// seconds; negatives clamp to zero (shouldn't happen with
    /// properly-ordered entries but defensive).
    ///
    /// Public so integration tests (`tests/prop_gap_mode.rs`) can
    /// assert the invariants via `proptest`. The function is pure and
    /// has no side effects — exposing it widens the API surface in
    /// name only.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn apply(&self, source_gap_secs: i64) -> i64 {
        let g = source_gap_secs.max(0);
        match self {
            Self::Exact => g,
            // Saturating cast on `cap.as_secs()` — `Duration::MAX`
            // is `u64::MAX` seconds; `as i64` wraps to a large
            // negative, which `g.min(...)` would then adopt,
            // nullifying the cap entirely. See R2 #2 for the
            // analogous bug on `time_budget` in `run()`.
            Self::Capped(cap) => g.min(i64::try_from(cap.as_secs()).unwrap_or(i64::MAX)),
            Self::Scaled(factor) => {
                let f = factor.clamp(0.0, 1.0);
                (g as f64 * f64::from(f)) as i64
            }
            Self::Tiered(tiers) => {
                // Find the first tier whose max_gap_secs >= g, apply
                // its speedup. Speedup >= 1.0 means compression: our
                // wait = g / speedup. Fallback tier = u64::MAX / 1.0
                // if list is empty or no tier matches.
                let speedup = tiers
                    .iter()
                    .find(|(max_gap, _)| (g as u64) <= *max_gap)
                    .map_or(1.0, |(_, s)| *s);
                let scaled = (g as f64) / speedup.max(0.000_001);
                // Clamp to i64 range (compression of huge gaps can
                // underflow to ~0, which is fine).
                if scaled > i64::MAX as f64 {
                    i64::MAX
                } else {
                    scaled as i64
                }
            }
        }
    }
}

/// User-facing speedup ladder, gated by per-gap and per-title-span
/// rules so naturally-fast pacing is never compressed.
///
/// **Holy-grail invariant: chronological order within a title is NEVER
/// broken regardless of which `MaxSpeedup` is selected.** The scheduler
/// always advances strictly monotonically (the `prev_entry_scheduled_ms
/// + 1ms` guard in `compute_schedule_for_title` runs AFTER speedup
/// scaling), so even at `Extreme100x` two consecutive entries cannot
/// land at the same instant.
///
/// Per-gap caps (the highest tier the user can demand at each gap size):
///
/// | gap            | max effective multiplier |
/// |----------------|--------------------------|
/// | < 1h           | 1.0× (verbatim — never compressed)        |
/// | 1h ≤ gap < 5h  | 2.0×                     |
/// | 5h ≤ gap < 30d | 3.0×                     |
/// | 30d ≤ gap < 1y | 5.0×                     |
/// | ≥ 1y           | 100.0×                   |
///
/// The user's `MaxSpeedup` choice clips the per-gap cap from above:
/// if you pick `Mild2x`, gaps of 30d-1y still get only 2× even though
/// their per-gap cap allows 5×. This reflects "the user wants an
/// upper bound on aggressiveness across the whole run".
///
/// Per-title rule: titles whose total source span (last unlock −
/// first unlock) is **less than 2 hours** are NEVER sped up — they
/// already finished naturally fast and compressing them looks bot-like.
///
/// # Composition with `GapMode`
///
/// `MaxSpeedup` runs **after** `apply_gap_mode_ms`, so the two
/// compose multiplicatively. Examples:
///
/// - `GapMode::Scaled(0.5)` (halve every gap) + `Mild2x` → effective
///   4× compression on gaps in the [1h, 5h) bucket.
/// - `GapMode::Tiered(PSICOTA)` (already up to 1_000_000× on >1y
///   gaps) + `Extreme100x` → 1e8× on year-plus gaps.
///
/// This is **intentional**: each layer represents a separate
/// concern (`GapMode` is "how do I shape the source's pacing
/// into a clone-able shape?", `MaxSpeedup` is "how aggressively
/// can I compress dormant gaps inside that shape?"). If you want
/// to opt out of a multiplicative composition, set one layer to
/// the identity (`GapMode::Exact` or `MaxSpeedup::Off`).
///
/// The `+1ms` monotonicity guard at the end of the chain runs
/// AFTER the composition, so even at million-x compression
/// chronological order is preserved by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxSpeedup {
    /// Off (1×) — replay every gap exactly as the source plan recorded.
    /// This is the default. The "santo graal" path: chronology is
    /// 100% faithful to the target's wall-clock.
    Off,
    /// Mild (2× cap). Gaps in [1h, 5h) compress 2×; gaps in
    /// [5h, 30d) compress 2× (capped from 3×); gaps ≥ 30d compress 2×
    /// (capped from 5×/100×). Sub-1h gaps untouched.
    Mild2x,
    /// Moderate (3× cap). Gaps [1h, 5h) compress 2× (per-gap rule);
    /// gaps [5h, 30d) compress 3×; gaps ≥ 30d compress 3× (capped
    /// from 5×/100×). Sub-1h gaps untouched.
    Moderate3x,
    /// Fast (5× cap). Gaps [1h, 5h) compress 2× (per-gap); [5h, 30d)
    /// compress 3× (per-gap); [30d, 1y) compress 5×; ≥ 1y compress 5×
    /// (capped from 100×). Sub-1h gaps untouched.
    Fast5x,
    /// Extreme (100× cap). Year-plus gaps compress dramatically
    /// (a year becomes ~3.6 days). Useful for cloning a multi-year
    /// trophy hunter's profile in a few weeks of wall-time. Use with
    /// care — extreme compression on aged profiles is a
    /// cheat-detection signal. Per-gap caps still apply: sub-1h gaps
    /// untouched, [1h, 5h) max 2×, etc.
    Extreme100x,
}

impl Default for MaxSpeedup {
    fn default() -> Self {
        // Default = Off. Honor the user's "santo graal" of perfect
        // chronological replay unless the user explicitly opts in.
        Self::Off
    }
}

// Custom Serialize/Deserialize so the wire shape uses the same
// human label set as the CLI/GUI: `"off"|"2x"|"3x"|"5x"|"100x"`.
// Auto-derive would emit `"Off"|"Mild2x"|...` — needlessly
// divergent from the user-visible labels.

impl Serialize for MaxSpeedup {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MaxSpeedup {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        // BUG HUNTER serde-MED-2 (wave 4): the previous impl
        // demanded a JSON STRING, which rejected `"max_speedup": 2`
        // (numeric form of the user-visible "2x" label). A
        // hand-written settings.json or a future schema rev that
        // emits unitless numbers got `invalid type: integer ...
        // expected a string`. Accept both:
        //   - JSON strings: `"off"`, `"2x"`, `"mild"`, ...
        //   - JSON numbers: `2`, `3`, `5`, `100` (interpreted as
        //     the matching `Nx` label; `1` and `0` map to `off`).
        // Bool / null / array / object stay as hard errors.
        let v = serde_json::Value::deserialize(de)?;
        let label = match &v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    // 0 and 1 both mean "no speedup"; everything
                    // else is treated as the `Nx` label.
                    if u <= 1 {
                        "off".to_string()
                    } else {
                        format!("{u}x")
                    }
                } else if let Some(i) = n.as_i64() {
                    if i <= 1 {
                        "off".to_string()
                    } else {
                        format!("{i}x")
                    }
                } else {
                    return Err(serde::de::Error::custom(format!(
                        "MaxSpeedup numeric value out of integer range: {n}"
                    )));
                }
            }
            _ => {
                return Err(serde::de::Error::custom(format!(
                    "MaxSpeedup must be a string or integer; got {v}"
                )));
            }
        };
        Self::from_label(&label).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown MaxSpeedup label `{label}` — accepted: off|1x, 2x|mild, 3x|moderate, 5x|fast, 100x|extreme"
            ))
        })
    }
}

impl MaxSpeedup {
    /// All variants in increasing order of compression aggression.
    /// Convenient for iterators that need to exercise every tier
    /// (round-trip serialization tests, "tier ladder must be
    /// monotonic" property checks, GUI dropdown population).
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Off,
            Self::Mild2x,
            Self::Moderate3x,
            Self::Fast5x,
            Self::Extreme100x,
        ]
    }

    /// User-facing label, kept stable across Rust/JS/CLI.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Mild2x => "2x",
            Self::Moderate3x => "3x",
            Self::Fast5x => "5x",
            Self::Extreme100x => "100x",
        }
    }

    /// Parse user-facing label (case-insensitive). Accepts the
    /// labels emitted by `as_str` plus a few common synonyms
    /// (`1x`/`none`/`off` all map to `Off`).
    #[must_use]
    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "1x" | "1" | "" => Some(Self::Off),
            "2x" | "2" | "mild" => Some(Self::Mild2x),
            "3x" | "3" | "moderate" => Some(Self::Moderate3x),
            "5x" | "5" | "fast" => Some(Self::Fast5x),
            "100x" | "100" | "extreme" => Some(Self::Extreme100x),
            _ => None,
        }
    }

    /// User's chosen max multiplier as an `f64`. The per-gap cap
    /// (computed from gap size) is `min`'d against this to produce
    /// the final multiplier for any given gap.
    #[must_use]
    pub fn user_max_multiplier(self) -> f64 {
        match self {
            Self::Off => 1.0,
            Self::Mild2x => 2.0,
            Self::Moderate3x => 3.0,
            Self::Fast5x => 5.0,
            Self::Extreme100x => 100.0,
        }
    }
}

/// Per-gap maximum allowed multiplier given gap duration in seconds.
/// This is the mechanic that protects natural intra-session pacing
/// while still letting a long dormant gap compress aggressively.
///
/// Returns `1.0` for `gap_secs ≤ 1h` (verbatim) and ramps up by
/// tier: 2× for gaps `> 1h`, 3× for `> 5h`, 5× for `> 30d`, 100×
/// for `> 1y`. Boundary semantics are STRICT-GREATER (the spec the
/// user repeatedly emphasised: "Speedups only allowed on gaps >1h,
/// >5h, >30d, >1y"). A gap of exactly 3600s stays at 1.0 — its
/// pacing is already that of active play, not a dormant period.
#[must_use]
pub fn per_gap_speedup_cap(gap_secs: i64) -> f64 {
    const ONE_HOUR: i64 = 3600;
    const FIVE_HOURS: i64 = 5 * 3600;
    const THIRTY_DAYS: i64 = 30 * 24 * 3600;
    const ONE_YEAR: i64 = 365 * 24 * 3600;
    // `<=` so the spec's strict-greater ladder lands exactly. Wave-4
    // audit F2 found `<` here, allowing a gap of EXACTLY 1h to
    // promote to the 2× tier — wrong side of the spec boundary.
    if gap_secs <= ONE_HOUR {
        1.0
    } else if gap_secs <= FIVE_HOURS {
        2.0
    } else if gap_secs <= THIRTY_DAYS {
        3.0
    } else if gap_secs <= ONE_YEAR {
        5.0
    } else {
        100.0
    }
}

/// **Two-hour completionist guard.** Titles whose total source span
/// (last unlock − first unlock) is below this threshold are never
/// speed-adjusted — they're already naturally fast (e.g. trash
/// 1000G titles cleared in 90 minutes). Compressing them past
/// natural pacing is a strong cheat-detection signal: a real player
/// can't unlock 32 ach in 18 minutes when the target took 1h45m.
pub const SPEEDUP_MIN_TITLE_SPAN_SECS: i64 = 2 * 3600;

/// **One-hour minimum gap.** Speedup never touches gaps smaller
/// than this. Two achievements 23 minutes apart represent active
/// gameplay; compressing that to 11 minutes makes the cadence
/// visibly artificial. We only compress dormant periods.
pub const SPEEDUP_MIN_GAP_SECS: i64 = 3600;

/// Compute the total source span (last unlock − first unlock) of
/// a title in seconds. Used by the speedup gate to identify
/// naturally-fast titles that should never be compressed.
///
/// Returns 0 if fewer than 2 entries have parseable timestamps.
/// Because `effective_speedup_multiplier` short-circuits on
/// `title_span_secs < SPEEDUP_MIN_TITLE_SPAN_SECS` (2h), a return
/// value of 0 is **strictly safer** than a real measurement —
/// unparseable plans become un-speedable on the title-span guard
/// alone. Any plan that wants speedup must have at least 2
/// parseable timestamps spanning ≥ 2 hours.
///
/// (A previous version of this rustdoc claimed the OPPOSITE — that
/// 0 "downgrades to always allow speedup". That was wrong. The
/// guard at `effective_speedup_multiplier` returns `1.0` when
/// `title_span_secs < SPEEDUP_MIN_TITLE_SPAN_SECS`, so 0 is a
/// guarantee of no compression.)
#[must_use]
pub fn compute_title_span_secs(entries: &[PlanEntry]) -> i64 {
    let mut min_s: Option<i64> = None;
    let mut max_s: Option<i64> = None;
    for e in entries {
        if let Some((s, _)) = crate::plan::runner::parse_iso_subsec_public(&e.time_unlocked) {
            min_s = Some(min_s.map_or(s, |m| m.min(s)));
            max_s = Some(max_s.map_or(s, |m| m.max(s)));
        }
    }
    match (min_s, max_s) {
        // BUG HUNTER bbb-span-overflow: saturating — `parse_iso` does not
        // bound the year, so two adversarial/hand-edited timestamps can
        // straddle the i64 range and a plain `b - a` overflows (debug
        // panic / release wrap), corrupting the 2h speedup guard.
        (Some(a), Some(b)) => b.saturating_sub(a).max(0),
        _ => 0,
    }
}

/// Compute the *effective speedup multiplier* for a single gap
/// given the user's `MaxSpeedup` and the title's total source span.
///
/// Returns `1.0` (no speedup) when:
/// - User picked `Off`
/// - Title's total span is below `SPEEDUP_MIN_TITLE_SPAN_SECS`
/// - Gap is below `SPEEDUP_MIN_GAP_SECS`
///
/// Otherwise returns `min(per_gap_cap(gap), user_max)`.
#[must_use]
pub fn effective_speedup_multiplier(
    user_max: MaxSpeedup,
    title_span_secs: i64,
    gap_secs: i64,
) -> f64 {
    if user_max == MaxSpeedup::Off {
        return 1.0;
    }
    // Spec rule: "Games with ≤2h completionist time CANNOT be sped
    // up." The user emphasized strict-≤ at the boundary — a title
    // whose span is EXACTLY 7200 seconds (2 hours on the dot) MUST
    // remain uncompressed. Earlier this used `<` so 7200 fell into
    // the "speedup allowed" branch. Wave-4 audit F1 caught the
    // off-by-one. (Same change at the summary's guard, line ~474.)
    if title_span_secs <= SPEEDUP_MIN_TITLE_SPAN_SECS {
        return 1.0;
    }
    // Spec rule: "Speedups only allowed on gaps >1h, >5h, >30d, >1y"
    // — strictly greater. A gap of EXACTLY 3600s must NOT be
    // compressed. Same `<` → `<=` correction.
    if gap_secs <= SPEEDUP_MIN_GAP_SECS {
        return 1.0;
    }
    per_gap_speedup_cap(gap_secs).min(user_max.user_max_multiplier())
}

/// Pacing window for a title whose playtime requirement is ALREADY MET.
///
/// When Xbox credits me more time on a game than the target took to
/// finish it, the floor has nothing left to enforce: the "never faster
/// than the speedrunner" guarantee is satisfied by the clock before the
/// run even starts. Subtracting still leaves `floor - already <= 0`,
/// which clamped to `min_game_seconds` (60 s) and then divided by the
/// achievement count — a burst.
///
/// So this case gets its own window rather than the generic minimum:
/// finish the title inside half an hour, with the unlocks spread over
/// it. Long enough that the timestamps read like a session, short
/// enough that a game you have already over-played isn't re-paced
/// across hours you don't owe.
pub const OVERSHOOT_PACE_WINDOW_SECS: u64 = 30 * 60;

/// Minimum wall-clock spacing between two unlocks of the same game.
///
/// The pacing window can legitimately collapse to `min_game_seconds`
/// in two ways: an aggressive `MaxSpeedup` tier, or a RESUMED game
/// where Xbox already credits more playtime than the target's floor.
/// Either way the window then divides by the achievement count, and
/// nothing else in the chain sets a lower bound — a 50-achievement
/// game landing in 60 s means one unlock every 1.2 s.
///
/// That is not "faster than the speedrunner" (the playtime requirement
/// is satisfied); it is a *timestamp* pattern no human produces, which
/// is the other half of what this tool is trying to avoid. The rate
/// limiter is not a substitute — it bottoms out at 150 ms and exists
/// to protect Xbox's budget, not the replay's credibility.
///
/// Only bites under pathological compression: a 40-hour game at 100×
/// still has a 24-minute window, far above `50 × 20 s`.
pub const MIN_SECS_PER_ACHIEVEMENT: u64 = 20;

/// Widen a pacing window so `n` unlocks cannot land closer together
/// than [`MIN_SECS_PER_ACHIEVEMENT`]. Returns `window` unchanged when
/// it is already roomy enough (the common case).
#[must_use]
pub fn pace_window_with_min_spacing(window_secs: u64, n_unlocks: usize) -> u64 {
    let needed = (n_unlocks as u64).saturating_mul(MIN_SECS_PER_ACHIEVEMENT);
    window_secs.max(needed)
}

/// Ceiling on a playtime floor GUESSED from a title's calendar span.
///
/// When the plan carries a real `MinutesPlayed` for a title, that value
/// is used verbatim — this cap never applies to it. It only bounds the
/// fallback for titles whose stat was absent (older plan, private
/// profile, live-fetch unavailable), where the runner otherwise treats
/// "first unlock → last unlock" as a completion time.
///
/// That fallback is only meaningful while the span is SHORT: 40
/// achievements over 6 hours really is about 6 hours of play. Over
/// months or years it stops measuring play and starts measuring how
/// long the target owned the game. A profile with three years between
/// a title's first and last unlock produced a floor of **1095 days** —
/// the run fired one achievement, scheduled the next for 438 days out,
/// and the forecast honestly reported 0 unlocks in the next 30 days.
///
/// One day of continuous play is past any realistic single session, so
/// beyond it the span carries no information worth pacing on.
pub const FLOOR_GUESS_CAP_SECS: u64 = 24 * 3600;

/// Bound a floor that was GUESSED from a calendar span. Real
/// `MinutesPlayed` must not be passed through here — it needs no cap.
#[must_use]
pub fn cap_guessed_floor_secs(span_secs: u64) -> u64 {
    span_secs.min(FLOOR_GUESS_CAP_SECS)
}

/// Compress a **sequential-mode playtime floor** by the user's
/// `MaxSpeedup` tier.
///
/// # Why this is NOT `effective_speedup_multiplier`
///
/// That function answers "how much may I compress a *dormant gap
/// between two unlocks*?" and its guards are calibrated for exactly
/// that: gaps ≤ 1h are active gameplay (never compressed) and the
/// per-gap ladder keys off absolute duration, topping out at 3× for
/// anything under 30 days.
///
/// A sequential **floor** is a different quantity: it is one game's
/// whole real completion time (Xbox `MinutesPlayed` of the target).
/// Feeding it to the gap ladder is a category error that makes the
/// feature look broken — a 90-minute game would return `1.0` (below
/// the 2h title-span guard) and a 10-hour game would cap at `3.0`,
/// so `Fast5x` and `Extreme100x` would be indistinguishable from
/// `Moderate3x` for every realistic floor.
///
/// Here the tier means what the label says: `2x` finishes the game in
/// half the target's real playtime, `100x` in a hundredth.
///
/// # The one hard guard
///
/// `min_game_seconds` (default 60) survives compression — it is the
/// backstop that stops a game 100%-ing instantly, and it is also why
/// the result can never be `0`. **`floor_secs == 0` is the
/// concurrent-mode sentinel**, not "no floor": returning it here
/// would silently switch the title into gap-mode scheduling with no
/// pacing and no playtime gate. The `.max(1)` is belt-and-braces for
/// a caller that passes `min_game_seconds == 0`.
///
/// # Honesty
///
/// Compressing the floor deliberately breaks the "never reach 100%
/// faster than the target's real playtime" guarantee — that is the
/// entire point of the knob, and it is why the default is
/// [`MaxSpeedup::Off`]. Callers are expected to surface the active
/// tier in the UI rather than let the run look faithful when it is
/// not. The pacing *shape* is unaffected: `floor_adjusted_targets`
/// normalises by `span.max(floor_ms)`, so a compressed floor scales
/// the whole window proportionally and keeps the target's cadence.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn floor_speedup_secs(floor_secs: u64, tier: MaxSpeedup, min_game_seconds: u64) -> u64 {
    let hard_min = min_game_seconds.max(1);
    if tier == MaxSpeedup::Off || floor_secs == 0 {
        // `Off` is a pure pass-through so an untiered run stays
        // byte-identical; `0` keeps the concurrent-mode sentinel.
        return floor_secs;
    }
    let divisor = tier.user_max_multiplier().max(1.0);
    // f64 is exact for every floor a real profile can produce
    // (MinutesPlayed maxes out far below 2^53 seconds).
    let compressed = (floor_secs as f64 / divisor).floor() as u64;
    compressed.max(hard_min)
}

/// Summary of how a `MaxSpeedup` tier will reshape a single
/// title's pacing. Used by the GUI / CLI to give the user a
/// pre-flight preview ("with `Mild2x`, this title's 14-day span
/// becomes 12 days — 1.17× compression").
#[derive(Debug, Clone, PartialEq)]
pub struct SpeedupSummary {
    /// The title's total source span in seconds (last unlock −
    /// first unlock). 0 if fewer than 2 parseable timestamps.
    pub source_span_secs: i64,
    /// The total span in seconds AFTER the speedup ladder runs
    /// against every gap in the title. Strictly ≤ `source_span_secs`.
    pub post_speedup_span_secs: i64,
    /// Mean effective multiplier `source_span / post_span`. `1.0`
    /// when the title is fully untouched (Off, sub-1h gaps, or
    /// span < 2h guard). `> 1.0` for compression.
    pub mean_compression: f64,
    /// Whether the 2-hour title-span guard suppressed compression
    /// entirely. `true` ⇔ no gap was compressed regardless of tier.
    pub short_title_guard_active: bool,
    /// Number of gaps that fell in each cap bucket. Index by bucket:
    /// `[sub_1h, 1h_to_5h, 5h_to_30d, 30d_to_1y, ge_1y]`. Lets the
    /// UI render "this title has 12 long-dormant gaps that will
    /// compress" without a separate scan.
    pub gap_buckets: [usize; 5],
}

/// Compute a `SpeedupSummary` for a title given its source entries
/// and the user's `MaxSpeedup`. Pure: deterministic, no I/O. The
/// caller (GUI preview, CLI dry-run, README example) decides how
/// to render the result.
#[must_use]
pub fn compute_speedup_summary(entries: &[PlanEntry], max_speedup: MaxSpeedup) -> SpeedupSummary {
    let source_span_secs = compute_title_span_secs(entries);
    // Mirror the `<=` guard from `effective_speedup_multiplier` so
    // `short_title_guard_active` agrees with the actual scheduler
    // decision at the 2h boundary (spec: ≤2h titles cannot be sped
    // up). Wave-4 audit F1.
    let short_title_guard_active = source_span_secs <= SPEEDUP_MIN_TITLE_SPAN_SECS;
    // BUG HUNTER bbb-summary-unsorted-gap-walk: mirror the defensive
    // stable-sort that BOTH schedulers apply at entry
    // (`compute_schedule_for_title_with_speedup` line ~3648,
    // `compute_tiered_schedule_with_speedup` line ~3979). Plan order is
    // NOT guaranteed chronological (loaded-from-disk / hand-edited JSON
    // / aggregate-of-N merges / source-profile clock skew), and this
    // preview must decompose the timeline into the SAME gaps the runner
    // will actually schedule. Without the sort, the raw slice-order
    // walk below (`cur > prev` skips negative gaps) produces wrong
    // gap_buckets / post_speedup_span / mean_compression on unsorted
    // input, and can even report `post_speedup_span_secs >
    // source_span_secs`, violating this struct's documented
    // "Strictly <= source_span_secs" invariant. Unparseable timestamps
    // sort to the END (same tuple key as the schedulers).
    let mut sorted: Vec<&PlanEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| {
        let parsed = crate::plan::runner::parse_iso_subsec_public(&e.time_unlocked)
            .map(|(s, n)| s.saturating_mul(1_000_000_000).saturating_add(i64::from(n)));
        (parsed.is_none(), parsed.unwrap_or(i64::MAX))
    });
    let mut gap_buckets = [0_usize; 5];
    let mut total_source_ms: i64 = 0;
    let mut total_post_ms: i64 = 0;
    let mut prev_ms: Option<i64> = None;
    for e in &sorted {
        let cur_ms =
            crate::plan::runner::parse_iso_subsec_public(&e.time_unlocked).map(|(s, n)| {
                s.saturating_mul(1000)
                    .saturating_add(i64::from(n / 1_000_000))
            });
        if let (Some(prev), Some(cur)) = (prev_ms, cur_ms) {
            if cur > prev {
                // BUG HUNTER bbb-summary-overflow: saturating subtraction
                // (operands are saturated-ms timestamps that can pin to
                // i64::MIN/MAX on adversarial years; `cur > prev` alone
                // does not bound the difference).
                let gap_ms = cur.saturating_sub(prev);
                let gap_secs = gap_ms.saturating_div(1000);
                // BUG HUNTER bbb-bucket-boundary: `<=` so a gap landing
                // EXACTLY on a tier edge is counted in the tier the
                // scheduler actually applies (`per_gap_speedup_cap` uses
                // `<=`). Strict `<` bucketed boundary gaps one tier too
                // high in the GUI/CLI preview.
                let bucket = if gap_secs <= 3600 {
                    0
                } else if gap_secs <= 5 * 3600 {
                    1
                } else if gap_secs <= 30 * 86400 {
                    2
                } else if gap_secs <= 365 * 86400 {
                    3
                } else {
                    4
                };
                gap_buckets[bucket] = gap_buckets[bucket].saturating_add(1);
                total_source_ms = total_source_ms.saturating_add(gap_ms);
                // Summary previews speedup without gap_mode, so the
                // source gap IS the shaped gap — pass it for both.
                total_post_ms = total_post_ms.saturating_add(apply_speedup_ms(
                    max_speedup,
                    source_span_secs,
                    gap_secs,
                    gap_ms,
                ));
            }
        }
        if cur_ms.is_some() {
            prev_ms = cur_ms;
        }
    }
    let post_speedup_span_secs = total_post_ms.saturating_div(1000);
    let mean_compression = if total_post_ms > 0 {
        (total_source_ms as f64) / (total_post_ms as f64)
    } else {
        1.0
    };
    SpeedupSummary {
        source_span_secs,
        post_speedup_span_secs,
        mean_compression,
        short_title_guard_active,
        gap_buckets,
    }
}

/// Apply effective speedup to a single gap (in **milliseconds**).
/// Returns the compressed gap in milliseconds. The +1ms monotonicity
/// guard is enforced separately by the caller's
/// `prev_entry_scheduled_ms` check, so this function CANNOT produce
/// a value that violates ordering — it just shrinks the gap.
///
/// The per-gap cap is classified on `classify_gap_secs` — the **source**
/// gap — while the compression divides `gap_ms` — the **shaped** gap
/// (post-`gap_mode`). This matters: under the default `Capped(30d)`,
/// every shaped gap is ≤30 days, so classifying on the shaped gap
/// capped every tier at 3× — a user who picked Fast5x/Extreme100x
/// silently got Moderate3x. Classifying on the source gap lets a
/// >30d/>1y source gap earn its 5×/100× tier even after gap_mode
/// clamps it. (`compute_speedup_summary` passes the same value for
/// both, since it previews speedup without gap_mode.)
#[must_use]
pub fn apply_speedup_ms(
    user_max: MaxSpeedup,
    title_span_secs: i64,
    classify_gap_secs: i64,
    gap_ms: i64,
) -> i64 {
    if user_max == MaxSpeedup::Off || gap_ms <= 0 {
        return gap_ms.max(0);
    }
    let mul = effective_speedup_multiplier(user_max, title_span_secs, classify_gap_secs);
    if mul <= 1.0 + f64::EPSILON {
        return gap_ms;
    }
    // Saturating cast back from f64. `gap_ms` is at most ~3e10 (a
    // year of milliseconds is 3.15e10), well within f64's exact
    // integer range (2^53 ≈ 9e15). Division by `mul` cannot produce
    // negative or NaN here because we gated on `gap_ms > 0` and
    // `mul > 1`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out = (gap_ms as f64 / mul).round() as i64;
    out.max(0)
}
