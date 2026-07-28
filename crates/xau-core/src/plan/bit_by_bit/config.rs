//! Runtime configuration for a bit-by-bit run: `BitByBitConfig`, `CatchUpMode`, and the concurrency ceiling.

use super::*;

/// Hard cap on simultaneous titles a bit-by-bit run touches in
/// parallel. Matches Xbox's observed presence-heartbeat ceiling —
/// the Xbox PC app never registers presence for more than 8 titles
/// at once, and the bit-by-bit runner keeps a presence heartbeat
/// going on the titles it's unlocking so event-based achievements
/// land reliably. Going above 8 silently drops the extra heartbeats
/// on the server side → event-path partial failures.
pub const MAX_SIMULTANEOUS_TITLES: usize = 8;

/// Tunable parameters for a bit-by-bit replay.
#[derive(Debug, Clone)]
pub struct BitByBitConfig {
    /// Caller's own xuid — where the unlocks will land.
    pub my_xuid: String,
    /// How target-to-user gap scaling works. See `GapMode` docs.
    /// Default `Capped(30 days)` to match historical behaviour.
    pub gap_mode: GapMode,
    /// Stop after N unlocks in this run. `0` = no cap. Counts
    /// CONFIRMED unlocks only — tasks that error don't consume
    /// budget (audit bug #4).
    ///
    /// # Best-effort, not atomic
    ///
    /// The cap is enforced in the drain loop via `abort_all()` as
    /// soon as `unlocked_budget_remaining` hits zero. Because the
    /// per-title unlocker dispatches a *batch* (event-based) or
    /// *chunk* (title-based) request that may already be in flight
    /// on the wire when the abort signal arrives, the final
    /// confirmed count can exceed `max_entries` by up to
    /// `UNLOCK_CHUNK_SIZE - 1` (today: 63 — see `xbox/constants.rs`) on a single trailing
    /// chunk. This is a fundamental property of async cancellation
    /// against an already-committed HTTP POST — Xbox credits the
    /// whole request atomically on its side regardless of what we
    /// do on ours. See audit bug BBB-#8 for why this is "document,
    /// don't fix": the alternative (sequential per-id POSTs) costs
    /// ~100× more HTTP round-trips and triggers rate limits much
    /// sooner, making the nominal cap *less* achievable in
    /// practice.
    ///
    /// Callers who need a hard cap should compute + enforce it at
    /// the plan-construction layer (trim the source set) rather
    /// than rely on the runner's in-flight abort.
    pub max_entries: usize,
    /// Cap on total gamerscore scored THIS run. `0` = no cap. Once the
    /// run's confirmed gamerscore reaches this, no NEW title is started
    /// — the games already in progress finish to 100% (so we never
    /// stop a game half-done), then the run ends. Best-effort like
    /// `max_entries`: every in-flight title's full G lands, so the
    /// total can overshoot by up to [`Self::sequential_window`] games'
    /// worth (one game at the default window of 1). The check also
    /// reads the REAPED counter, so G from a game still in flight is
    /// not yet visible to it — another reason the bound is "up to N
    /// games", not "exactly the cap".
    pub max_gamerscore: u64,
    /// Only process the first `top_n` titles of the fastest-G-rate
    /// ranking (highest gamerscore-per-second of target wall-clock
    /// first — see `Plan::title_spans_fastest_gs_first`). No
    /// completion filter: partially-completed titles compete on
    /// their rate like everyone else. `0` = no cap — every title
    /// the caller selected is considered. Classic use: `top_n = 8`
    /// means "start with the 8 titles that earn G fastest" (matches
    /// MAX_SIMULTANEOUS_TITLES).
    pub top_n_titles: usize,
    /// When `Some`, the run only touches these title_ids. Takes
    /// precedence over `top_n_titles` — a caller who hand-picks
    /// titles with the GUI opts out of the ranking. Empty set
    /// behaves like `None` (no filter).
    pub selected_titles: Option<std::collections::HashSet<String>>,
    /// Titles to process in parallel. Clamped to
    /// `MAX_SIMULTANEOUS_TITLES` (=8) at runtime — Xbox's presence
    /// ceiling. Default 6 to match historical rate-limit math.
    pub title_concurrency: usize,
    /// Optional user-facing time budget. The scheduler still
    /// computes per-entry scheduled times normally, but entries
    /// whose `scheduled_unix > now + time_budget` are deferred to
    /// the NEXT run. `None` = no budget = process everything the
    /// scheduler says is eligible now. Used together with the
    /// estimator UI: "if I run bit-by-bit for 6 hours, how much
    /// will I unlock?"
    pub time_budget: Option<Duration>,
    /// Override `UNLOCK_CHUNK_SIZE`. `Some(1)` = strict per-ach POSTs
    /// so timestamps inside a chunk are strictly monotonic — matters
    /// here because we're carefully ordering things, and a single
    /// chunk POST collapses multiple timestamps into one.
    pub chunk_size_override: Option<usize>,
    /// Unlock-safety policy. Default strict — revoked + time-window
    /// violations are never unlocked, even if the source plan
    /// somehow contains them (stale plan file from before the filter
    /// was added, for example).
    pub safety: crate::plan::safety::SafetyPolicy,
    /// Keep a presence heartbeat going on selected titles while the
    /// run is active. Required for event-based titles (OneCollector
    /// checks recent presence), harmless for title-based. Default
    /// true. Caller pays ~1 bulk POST every 55s of wall time.
    pub presence_keepalive: bool,
    /// "Shadow replay" mode — max number of titles that can be in
    /// "in progress" state on the user's profile at any given time.
    /// A title counts as in-progress when the user has ≥1 unlock on
    /// it but fewer than the target's total. When this cap is set
    /// (and not zero), the dispatch loop will NOT start processing a
    /// "not yet started" title (one where user has 0 unlocks) unless
    /// the current in-progress count is below this cap.
    ///
    /// Typical value: 2-8 for a "dedicated hunter juggling N games".
    /// `0` = no cap (classic bit-by-bit behaviour).
    ///
    /// Combined with `GapMode::Tiered`, this creates a fully
    /// resumable "shadow replay" run: user plays, closes, re-runs
    /// tomorrow → Xbox still shows N titles in progress, new titles
    /// only promoted when old ones complete.
    pub active_slots_cap: usize,
    /// User's chosen max speedup tier — applied per-gap with caps
    /// (see `MaxSpeedup` rustdoc). Default `Off` (1× verbatim).
    /// Composes with `gap_mode` multiplicatively: `gap_mode` runs
    /// first, then speedup compresses the result.
    pub max_speedup: MaxSpeedup,
    /// How to handle entries whose scheduled time is already in the
    /// PAST at the start of a run (the resume-after-absence case). See
    /// [`CatchUpMode`]. Default [`CatchUpMode::Shift`] — the safe one:
    /// it never fires a pile of overdue unlocks back-to-back.
    pub catch_up: CatchUpMode,
    /// **Sequential "one game at a time to 100%" mode.** DEFAULT ON.
    ///
    /// When `false` the runner dispatches every eligible title
    /// CONCURRENTLY (up to `title_concurrency`), so unlocks from
    /// different games interleave on the wire and Xbox presence shows
    /// several games "in progress" at once — the classic fast path
    /// (opt in via `--concurrent` / unchecking the GUI toggle).
    ///
    /// When `true` (default), titles are processed
    /// [`Self::sequential_window`] at a time (default `1` = strictly
    /// one), each fully to 100% before the next starts:
    /// 1. go ONLINE on the game (presence heartbeat),
    /// 2. fire ALL that game's not-already-done achievements IN ORDER,
    ///    paced evenly across the game's minimum plausible completion
    ///    time (`floor` = the target's own first→last-unlock span from
    ///    the plan, but at least [`Self::min_game_seconds`]), so the
    ///    game can never be 100%'d faster than a real fast human,
    /// 3. go OFFLINE for that game (presence stop), THEN move on.
    ///
    /// In this mode the per-game floor REPLACES the `gap_mode`/
    /// `time_budget` scheduling for the active game (the whole game is
    /// completed this run at floor pace), and the unscoped presence
    /// stop is safe because exactly one game is ever active. Do NOT run
    /// spoof-farm concurrently with a sequential run — the per-game
    /// offline would tear down its presence too.
    pub sequential: bool,
    /// **How many games [`Self::sequential`] mode keeps in flight at
    /// once.** Default `1` — classic "one game at a time to 100%".
    ///
    /// Clamped to `1..=MAX_SIMULTANEOUS_TITLES` at runtime. Ignored
    /// when `sequential` is false (the concurrent path is bounded by
    /// [`Self::title_concurrency`] instead).
    ///
    /// # Why this is a separate field from `title_concurrency`
    ///
    /// `title_concurrency` defaults to **6** and bounds *per-POST*
    /// permits. Reusing it here would silently flip every existing
    /// sequential user from 1 game to 6 on upgrade, with no action on
    /// their part — a change to the one behaviour the mode is named
    /// after. This field starts at the current behaviour and only
    /// moves when the user asks.
    ///
    /// # What `> 1` actually means
    ///
    /// It is a THIRD mode, not "sequential but faster": N games are
    /// each independently completed to 100% at their own playtime
    /// floor, with overlapping presence and overlapping floor gates.
    /// Two consequences the caller must own:
    /// - **Playtime arithmetic.** N games in flight accrue ~N minutes
    ///   of `MinutesPlayed` per elapsed minute across the profile,
    ///   which is not something one person at one console can do. It
    ///   is a larger effective speedup than the entire
    ///   [`MaxSpeedup`] ladder can legally reach.
    /// - **Presence.** The per-game go-offline is an UNSCOPED delete,
    ///   so it only runs at `1` (see the dispatch loop); above that,
    ///   presence lapses via Xbox's TTL as each title leaves the live
    ///   keepalive set.
    pub sequential_window: usize,
    /// Floor (seconds) on how long a single game may take to reach
    /// 100% in [`Self::sequential`] mode. Guards "burst" titles whose
    /// plan span is ~0 (single-achievement games, or all achievements
    /// stamped the same second) from completing instantly. The
    /// effective per-game pace is `max(plan_span_secs, min_game_seconds)`.
    /// Ignored when `sequential` is false. Default 60.
    pub min_game_seconds: u64,
}

/// What to do with entries that are already overdue when a run starts
/// (you closed the app for a while, came back, pressed Start).
///
/// After the anchor-clamp removal made the scheduler resume from the
/// user's TRUE historical unlock time, a long absence leaves many
/// entries with a `scheduled_unix` in the past — all simultaneously
/// eligible. Firing them as-is means they land back-to-back at the
/// rate-limiter's ~150 ms spacing: gaps that were days apart in the
/// source collapse into one second. That is both the biggest fidelity
/// loss AND a textbook bot fingerprint ("30 achievements in 5 seconds
/// after 3 weeks idle"). `Shift` is the default so this never happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CatchUpMode {
    /// Deficit-shift (default, SAFE): slide the whole remaining chain
    /// forward so the earliest overdue entry lands at `now` and every
    /// subsequent inter-unlock gap is preserved VERBATIM going forward.
    /// One entry fires now; the rest fire at their real cadence over
    /// the coming minutes/hours/days (bounded by `time_budget`). Never
    /// produces a burst; wire order is unchanged.
    #[default]
    Shift,
    /// Burst (opt-in): fire every overdue entry immediately, paced only
    /// by the rate limiter. Fastest, but collapses real gaps and looks
    /// bot-like. Only for throwaway accounts where speed beats realism.
    Burst,
}

impl Default for BitByBitConfig {
    fn default() -> Self {
        Self {
            my_xuid: String::new(),
            gap_mode: GapMode::Capped(Duration::from_secs(30 * 24 * 60 * 60)),
            max_entries: 0,
            max_gamerscore: 0,
            top_n_titles: 0,
            selected_titles: None,
            title_concurrency: 6,
            time_budget: None,
            // Default to chunk_size=1 for this mode — preserving the
            // target's per-game order is the whole point, so we pay
            // the request overhead to get strict monotonic timestamps.
            chunk_size_override: Some(1),
            safety: crate::plan::safety::SafetyPolicy::strict(),
            presence_keepalive: true,
            active_slots_cap: 0,
            max_speedup: MaxSpeedup::default(),
            catch_up: CatchUpMode::default(),
            // Sequential is the DEFAULT: finish one game to 100% (online
            // → paced → offline) before the next. Opt OUT for the fast
            // concurrent path. See the field docs.
            sequential: true,
            // ONE game at a time — the historical (and only ever
            // shipped) sequential behaviour. Never default this from
            // `title_concurrency`: see the field docs.
            sequential_window: 1,
            min_game_seconds: 60,
        }
    }
}

/// Canned tier-speedup presets for the shadow-replay mode. See
/// `GapMode::Tiered` rustdoc for the semantics. Each preset returns
/// a full `BitByBitConfig` tuned for its use case — caller still
/// supplies `my_xuid` before `start_bit_by_bit`.
///
/// Boundary units: seconds. The final `u64::MAX` tier catches
/// anything above ~1 year.
pub mod shadow_presets {
    use super::{BitByBitConfig, GapMode};

    /// "Psicota real" — max throughput within believability. 8
    /// parallel games, bursts preserved at 1x, medium gaps at 50x,
    /// dormant (>3d) crushed by 1000x+. Total clone ~6 months.
    #[must_use]
    pub fn psicota() -> BitByBitConfig {
        BitByBitConfig {
            gap_mode: GapMode::Tiered(vec![
                (3600, 1.0),
                (7200, 2.0),
                (259_200, 50.0),
                (2_592_000, 1000.0),
                (7_776_000, 100_000.0),
                (31_104_000, 1_000_000.0),
                (u64::MAX, 10_000_000.0),
            ]),
            active_slots_cap: 8,
            title_concurrency: 8,
            ..Default::default()
        }
    }

    /// "New account" — slightly more conservative than psicota, 6
    /// slots, medium compression. Total clone ~12 months.
    #[must_use]
    pub fn new_account() -> BitByBitConfig {
        BitByBitConfig {
            gap_mode: GapMode::Tiered(vec![
                (3600, 1.0),
                (7200, 1.5),
                (259_200, 20.0),
                (2_592_000, 500.0),
                (7_776_000, 50_000.0),
                (u64::MAX, 1_000_000.0),
            ]),
            active_slots_cap: 6,
            title_concurrency: 6,
            ..Default::default()
        }
    }

    /// "Max speed" — bursts fiel + everything else instant. Only
    /// bottleneck is Xbox's rate limit. Total clone ~6 months
    /// (bounded by rate limit, not by pacing).
    #[must_use]
    pub fn max_speed() -> BitByBitConfig {
        BitByBitConfig {
            gap_mode: GapMode::Tiered(vec![(3600, 1.0), (u64::MAX, 1_000_000.0)]),
            active_slots_cap: 8,
            title_concurrency: 8,
            ..Default::default()
        }
    }
}
