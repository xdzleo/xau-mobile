// Module-level allow list:
// - `too_many_lines`: `run()` intentionally walks resolve → schedule
//   → execute top-to-bottom for readability.
// - `items_after_statements`: the inner EntrySchedule struct is
//   scoped next to its only user.
// - `cast_possible_wrap`: u64 unix seconds → i64 fits well under
//   i64::MAX (year 292B).
// - `assigning_clones`: shorter + equally efficient for small fields.
// - `needless_update`: test helpers use `..Default::default()` on
//   fully-specified struct literals so adding a field to `PlanEntry`
//   doesn't break every test at once.
#![allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::cast_possible_wrap,
    clippy::assigning_clones,
    clippy::needless_update
)]

//! "Bit-by-bit per-game chronological replay" — the most faithful
//! replay mode.
//!
//! # What it does
//!
//! For each title in the source plan, it replays achievements **in
//! the target's original order**, preserving the **real-world gap**
//! between consecutive unlocks (clamped to a configurable maximum,
//! default 30 days). Runs **incrementally / resumably** — each
//! execution checks the user's OWN current unlocks and only fires
//! achievements whose scheduled time has already passed.
//!
//! # Why incrementally
//!
//! If the target profile unlocked Achievement A on day 1 and
//! Achievement B on day 301, a "faithful mirror" replay would
//! literally wait 300 days. That's useless. We cap each gap at
//! `max_gap` (default 30 days), so:
//!
//! ```text
//!   Target: A @ 2024-01-01, B @ 2024-10-27 (300 days later)
//!   You:    A @ now,        B @ now + 30 days (clamped)
//! ```
//!
//! When you run the plan again tomorrow, it sees "A was unlocked 24h
//! ago; B needs 30 days between A and B; not yet; skip". Run it again
//! in 30 days, it sees the gap has elapsed and fires B.
//!
//! # Why resumable
//!
//! Plans are stateless. Each run:
//! 1. Fetches YOUR OWN achievements via the bulk scan endpoint.
//! 2. For each source achievement: check if you already have it.
//! 3. If not, compute the scheduled unlock time based on your
//!    last-unlocked-in-this-title + effective gap.
//! 4. If `now >= scheduled`, unlock. Else skip.
//!
//! No database, no state file, no "where did I stop last time" —
//! your profile's server-side `timeUnlocked` values ARE the state.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::error::{Error, Result};
use crate::plan::runner::parse_iso_public as parse_iso;
use crate::plan::{Plan, PlanEntry};
use crate::xbox::XboxClient;

// ===== Module tree (each submodule owns one responsibility; see its
// //! doc). Split out of a single 8.5k-line file for navigability. =====
mod config;
mod dispatch;
mod floor_gate;
mod gap_speedup;
mod logging;
mod pacing;
mod presence;
mod process_title;
mod progress;
mod scheduling;
#[cfg(test)]
mod tests;
mod time;

// Private globs so sibling submodules reach each other via `use super::*`
// (each brings a module's pub(super) internals into this facade so a
// sibling's `use super::*` can see them). Only modules with pub(super)
// items consumed by a SIBLING are listed — pub items reach siblings via
// the `pub use` re-exports below, so a redundant glob would just warn.
use dispatch::*;
use floor_gate::*;
use logging::*;
use pacing::*;
use process_title::*;
use progress::*;
use scheduling::*;
use time::*;

// Public API — re-exported UNCHANGED for the rest of the workspace.
pub use config::{shadow_presets, BitByBitConfig, CatchUpMode, MAX_SIMULTANEOUS_TITLES};
pub use dispatch::start_bit_by_bit;
pub use gap_speedup::{
    apply_speedup_ms, cap_guessed_floor_secs, compute_speedup_summary, compute_title_span_secs,
    effective_speedup_multiplier, floor_speedup_secs, pace_window_with_min_spacing,
    per_gap_speedup_cap, GapMode, MaxSpeedup, SpeedupSummary, FLOOR_GUESS_CAP_SECS,
    MIN_SECS_PER_ACHIEVEMENT, OVERSHOOT_PACE_WINDOW_SECS, SPEEDUP_MIN_GAP_SECS,
    SPEEDUP_MIN_TITLE_SPAN_SECS,
};
pub use pacing::{UNLOCK_RATE_LIMIT_PERIOD_MS, UNLOCK_RATE_LIMIT_REQUESTS};
pub(crate) use presence::{presence_keepalive_loop, PresenceSender};
pub use progress::{
    ActiveTitleLive, BitByBitHandle, BitByBitProgress, BitByBitState, LiveControls, RecentUnlock,
    SkippedGame,
};
pub use scheduling::{
    apply_catch_up_shift, compute_schedule_for_title, compute_schedule_for_title_with_speedup,
    compute_tiered_schedule, compute_tiered_schedule_with_speedup, ScheduledEntry,
};
