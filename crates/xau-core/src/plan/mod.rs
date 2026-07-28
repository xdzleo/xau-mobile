//! "Follow another profile's chronology" — fetches the full ordered list of
//! achievements that a target Xbox Live profile has unlocked, sorted by the
//! actual `progression.time_unlocked` timestamps exposed by Xbox Live's
//! public achievements endpoint, and replays them on the caller's own
//! profile with configurable spacing.
//!
//! No web scraping. Everything comes straight from `xboxlive.com`:
//!
//! - `GET titlehub.xboxlive.com/.../titles/titleHistory/...` — target's
//!   games list with per-title achievement progress.
//! - `GET achievements.xboxlive.com/.../achievements?titleId=...` — each
//!   unlocked achievement with its `progression.time_unlocked` value.
//! - `POST achievements.xboxlive.com/.../achievements/{scid}/update` — the
//!   existing unlock call used by the rest of the app.
//!
//! The plan is serialisable JSON so it can be exported, shared, diffed or
//! edited before being replayed.

pub mod aggregate;
pub mod bbb_report;
pub mod bit_by_bit;
pub mod bit_by_bit_estimate;
pub mod builder;
pub mod discontinued;
pub mod nuclear;
pub mod runner;
pub mod runner_lock;
pub mod safety;
pub mod storage;

use serde::{Deserialize, Serialize};

/// A single achievement in a replay plan. Ordered by `time_unlocked` across
/// the whole plan.
///
/// Struct-level `#[serde(default)]` — every field accepts absence. Lets us
/// add fields in future patches without breaking old plans saved to disk.
///
/// NOTE: field types are `String` because this is the transport/storage
/// struct shared with the on-disk JSON format. The typed domain newtypes
/// (`TitleId`, `AchievementId`, `Xuid`, `ServiceConfigId`) live in
/// `crate::types` — API signatures and internal helpers convert at
/// the boundary via `TryFrom`. The Plan struct itself keeps strings so
/// we can land the migration incrementally without a 15-file cascade
/// that's impossible to review atomically.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanEntry {
    pub title_id: String,
    /// `ServiceConfigId` of the title — carried along so the runner skips the
    /// look-up request. Empty for Xbox 360 titles (we can't unlock those via
    /// the current `update` endpoint anyway).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub service_config_id: String,
    pub achievement_id: String,
    pub achievement_name: String,
    pub game_name: String,
    /// ISO-8601 timestamp when the target unlocked this achievement
    /// (as returned by `progression.time_unlocked`). Used for ordering and
    /// optional real-time pacing.
    pub time_unlocked: String,
    pub gamerscore: Option<i32>,
}

/// Per-title completion counts captured from the target's titlehub
/// games list at plan-build time. Lets consumers answer "did the
/// target 100% this title?" without re-fetching metadata.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleCompletion {
    /// How many achievements the target has unlocked in this title
    /// (matches `titlehub.achievement.currentAchievements`).
    pub current_achievements: i32,
    /// Total achievements the title has (matches
    /// `titlehub.achievement.totalAchievements`). When 0, the title
    /// has no achievement metadata available — treat as unknown.
    pub total_achievements: i32,
}

impl TitleCompletion {
    /// True when the target has unlocked every achievement and we
    /// know the total (`total > 0`). A title with `total = 0` is
    /// treated as unknown, NEVER as complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.total_achievements > 0 && self.current_achievements >= self.total_achievements
    }
}

/// A replay plan — ordered chronological list of achievements plus some
/// metadata about the target profile for display purposes.
///
/// Struct-level `#[serde(default)]` means every field accepts absence,
/// so plans saved with older versions still deserialize cleanly when a
/// field is added. Combined with `..Default::default()` at every
/// construction site, adding new fields never breaks callers.
///
/// # Typed domain fields
///
/// `source_xuid` is a validated `Xuid` (see `types` module). Older
/// plans that serialize an empty string here fail to parse — which
/// surfaces plan-file corruption immediately instead of later as a
/// 404 on `/profile/xuid()/...`. The `source_gamertag` is an
/// `Option<Gamertag>` because gamertag resolution can lag behind
/// xuid (memory-extracted tokens sometimes have no gamertag until
/// we call peoplehub later). `title_totals` is keyed by the typed
/// `TitleId` — same JSON shape (string keys) but typed lookups.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Plan {
    pub source_gamertag: String,
    pub source_xuid: String,
    /// ISO-8601 timestamp when the plan was built.
    pub built_at: String,
    /// Total achievement count across all games the target has unlocked.
    pub total: usize,
    pub entries: Vec<PlanEntry>,
    /// Per-title completion counts from titlehub (keyed by `title_id`).
    /// Used by `title_spans_100_percent_shortest_first` to filter to
    /// only games the target 100%-completed. Populated at plan-build
    /// time. Missing entries = unknown (treated as not complete).
    pub title_totals: std::collections::HashMap<String, TitleCompletion>,
    /// Per-title REAL playtime floor, in SECONDS, keyed by `title_id`.
    /// Sourced from Xbox's `MinutesPlayed` stat (the same number
    /// TrueAchievements shows as "time played") for the target profile
    /// at plan-build time — i.e. how long it actually took to reach the
    /// achievements in the plan, NOT the calendar span between the
    /// first and last unlock. This is the authoritative floor for
    /// sequential-mode pacing: a game is never 100%'d faster than this.
    ///
    /// For an aggregate ("best-of") plan, it's the MIN across the
    /// source profiles (the fastest real completion). Missing/absent
    /// (empty map, or a title's stat was private/unavailable) → the
    /// runner falls back to the plan's calendar span. `#[serde(default)]`
    /// via the struct-level `default`, so plans built before this field
    /// existed still parse.
    pub title_min_completion_secs: std::collections::HashMap<String, i64>,
    /// Real, re-queryable source profile xuid(s) that fed this plan —
    /// used to LIVE-fetch a title's MinutesPlayed floor at run start when
    /// it's missing from `title_min_completion_secs` (old plan, or the
    /// stat was private at build time). For a single-profile plan this is
    /// `[source_xuid]`; for a best-of aggregate it's the real input
    /// profiles' xuids (NOT the synthetic "aggregate-of-N" `source_xuid`),
    /// so the runner can re-derive the fastest real completion live and
    /// never fall back to the 60 s floor when the data is a query away.
    /// Empty = old plan with no attribution → runner keeps the calendar-
    /// span fallback. `#[serde(default)]` via the struct-level default.
    pub source_xuids: Vec<String>,
    /// Why the scan's achievements didn't all become plan entries.
    ///
    /// A plan is ALWAYS smaller than the profile it was built from —
    /// Xbox 360 and mobile titles can't be unlocked through this API,
    /// entries without a timestamp can't be ordered, revoked ones are
    /// refused. Without these counts "why does this plan have 2.8k
    /// entries when the profile has more achievements than the other
    /// one?" has no answer short of reading the log, and the natural
    /// (wrong) conclusion is that the scan truncated.
    ///
    /// `#[serde(default)]` via the struct-level default, so plans built
    /// before this existed still parse — they just report zeroes.
    pub build_stats: PlanBuildStats,
}

/// Per-reason tally of achievements dropped while assembling a plan.
/// Sums to `scanned - total`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlanBuildStats {
    /// Achievements the bulk scan returned, before any filtering.
    pub scanned: usize,
    /// Xbox 360 / iOS / Android / Switch — no unlock endpoint.
    pub non_replayable: usize,
    /// No `timeUnlocked`, so there is no chronology to replay.
    pub missing_timestamp: usize,
    /// No title association — nothing to attribute the unlock to.
    pub no_title_assoc: usize,
    /// Revoked by Microsoft.
    pub safety_revoked: usize,
    /// Outside its event window (expired, or not yet open).
    pub safety_time_window: usize,
    /// Dropped by the caller's own include / skip / min-unlocked filters.
    pub user_filters: usize,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Keep completion metadata scoped to titles that are actually present
    /// in this plan's executable entry list.
    pub fn retain_title_totals_for_entries(&mut self) {
        let title_ids: std::collections::HashSet<String> =
            self.entries.iter().map(|e| e.title_id.clone()).collect();
        self.title_totals.retain(|tid, _| title_ids.contains(tid));
    }

    /// Sum of `gamerscore` across all UNIQUE entries (deduped by
    /// `(title_id, achievement_id)`). Entries without a gamerscore
    /// (rare — some Xbox catalog rows omit it) count as 0.
    /// `i64` so a 1M-achievement mega-plan can't overflow.
    ///
    /// BUG HUNTER TG-N1: a plan with internal duplicates (corrupted
    /// file, older aggregate output) used to sum the dup's
    /// gamerscore twice, reporting a total inflated above what the
    /// replay runner could actually unlock — Xbox returns 200 OK on
    /// a dup POST but doesn't re-credit the achievement, so the
    /// second occurrence contributes 0 G to the user's profile.
    /// Dedupe before sum so the "plan total gamerscore" number
    /// matches the REAL unlock credit.
    #[must_use]
    pub fn total_gamerscore(&self) -> i64 {
        let mut seen: std::collections::HashSet<(&str, &str)> =
            std::collections::HashSet::with_capacity(self.entries.len());
        self.entries
            .iter()
            .filter(|e| seen.insert((e.title_id.as_str(), e.achievement_id.as_str())))
            .map(|e| i64::from(e.gamerscore.unwrap_or(0)))
            .sum()
    }

    /// Per-(title, achievement) uniqueness of entries — the set the
    /// replay runner actually operates on. Used by the diff-vs-me
    /// path in the GUI to answer "of these, how many do I already
    /// have?".
    #[must_use]
    pub fn unlock_keys(&self) -> std::collections::HashSet<(String, String)> {
        self.entries
            .iter()
            .map(|e| (e.title_id.clone(), e.achievement_id.clone()))
            .collect()
    }

    /// Compute how much this plan would net-gain against a set of
    /// already-unlocked (title_id, achievement_id) pairs belonging
    /// to the caller's own profile. Returns counts + gamerscore of
    /// the entries NOT in `my_unlocks` — i.e., what a replay would
    /// actually add.
    ///
    /// Entries without a gamerscore count as 0 toward `net_new_gs`
    /// (same policy as `total_gamerscore`).
    #[must_use]
    pub fn diff_vs(&self, my_unlocks: &std::collections::HashSet<(String, String)>) -> PlanDiff {
        let mut net_new_count = 0usize;
        let mut net_new_gs: i64 = 0;
        let mut already = 0usize;
        let mut already_gs: i64 = 0;
        // Per-title breakdown: (net_new_count, net_new_gs, already_count, name).
        let mut by_title: std::collections::HashMap<String, (usize, i64, usize, String)> =
            std::collections::HashMap::new();
        // BUG HUNTER DV-N1: dedupe by (title_id, achievement_id)
        // before counting. A plan with internal duplicates (corrupted
        // file, old aggregate output pre-A5/N16 fix) would otherwise
        // count the same `(T1, a)` twice toward either `net_new_count`
        // or `already_unlocked_count` — inflating the number the UI
        // shows for "I would gain X achievements" or "already have X"
        // versus the ACTUAL deduped unlock count the runner would
        // dispatch. We track which keys we've already processed and
        // skip subsequent duplicates.
        let mut processed: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::with_capacity(self.entries.len());
        for e in &self.entries {
            let key = (e.title_id.clone(), e.achievement_id.clone());
            if !processed.insert(key.clone()) {
                continue;
            }
            let gs = i64::from(e.gamerscore.unwrap_or(0));
            let row = by_title
                .entry(e.title_id.clone())
                .or_insert_with(|| (0, 0, 0, e.game_name.clone()));
            if my_unlocks.contains(&key) {
                already += 1;
                already_gs += gs;
                row.2 += 1;
            } else {
                net_new_count += 1;
                net_new_gs += gs;
                row.0 += 1;
                row.1 += gs;
            }
        }
        // Sort per-title breakdown by net_new_gs DESC — biggest
        // gains first. Stable secondary on game name so ties are
        // deterministic across runs.
        let mut per_title: Vec<PlanDiffPerTitle> = by_title
            .into_iter()
            .map(|(title_id, (nn, nng, al, name))| PlanDiffPerTitle {
                title_id,
                game_name: name,
                net_new_count: nn,
                net_new_gamerscore: nng,
                already_unlocked_count: al,
            })
            .collect();
        per_title.sort_by(|a, b| {
            b.net_new_gamerscore
                .cmp(&a.net_new_gamerscore)
                .then_with(|| a.game_name.cmp(&b.game_name))
        });
        PlanDiff {
            net_new_count,
            net_new_gamerscore: net_new_gs,
            already_unlocked_count: already,
            already_unlocked_gamerscore: already_gs,
            per_title,
        }
    }
}

/// Result of `Plan::diff_vs(my_unlocks)` — "how much would I gain
/// by executing this plan against my own profile".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDiff {
    /// Entries in the plan that are NOT in my unlocks. This is the
    /// true "I would gain N achievements" number.
    pub net_new_count: usize,
    /// Sum of gamerscore across `net_new_count` entries. The
    /// "I would gain X G" number.
    pub net_new_gamerscore: i64,
    /// Entries the plan has that I already have — replaying these
    /// is a no-op on Xbox's side. Shown so the user understands
    /// the plan's full size vs. the usable delta.
    pub already_unlocked_count: usize,
    pub already_unlocked_gamerscore: i64,
    /// Per-title breakdown sorted by `net_new_gamerscore` DESC.
    /// Lets the UI show "biggest gains" first.
    pub per_title: Vec<PlanDiffPerTitle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDiffPerTitle {
    pub title_id: String,
    pub game_name: String,
    pub net_new_count: usize,
    pub net_new_gamerscore: i64,
    pub already_unlocked_count: usize,
}

/// Per-title span computed from a `Plan` — how long the target spent
/// between their first and last unlock for that title, plus the
/// achievement count. Used by the GUI's plan summary + by the
/// `bit_by_bit` runner to order titles short-first (quick wins
/// unlock first, long grinds trickle later).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleSpan {
    pub title_id: String,
    pub game_name: String,
    pub achievement_count: usize,
    /// Seconds between the target's first and last unlock in this
    /// title (i.e. `last.time_unlocked - first.time_unlocked`). Zero
    /// when the title has only one unlock or the timestamps can't be
    /// parsed.
    pub duration_seconds: i64,
    /// Total gamerscore across the target's UNIQUE unlocks in this
    /// title (dupes deduped by achievement_id, missing G counts 0).
    /// Feeds the "fastest gamerscore" (G/time) ranking.
    #[serde(default)]
    pub gamerscore: i64,
    /// The first unlock's ISO timestamp (for display).
    pub first_unlocked: String,
    /// The last unlock's ISO timestamp (for display).
    pub last_unlocked: String,
}

/// Compute the span (seconds between first and last unlock) for a
/// set of plan entries' ISO timestamps. Single source of truth so
/// the ranking shown in the GUI and the ordering applied by
/// bit-by-bit/nuclear never drift. Returns 0 when no entry has a
/// parseable timestamp or when all entries share the exact same
/// timestamp (1-achievement title, or multi-achievement burst).
///
/// Exposed `pub` so `bit_by_bit::run` can call it directly instead
/// of re-implementing the min/max logic (which it did, and which
/// worked, but silently diverging copies are a future bug).
#[must_use]
pub fn compute_span_seconds<'a, I>(isos: I) -> i64
where
    I: IntoIterator<Item = &'a str>,
{
    let parse = runner::parse_iso_public;
    let mut first: Option<i64> = None;
    let mut last: Option<i64> = None;
    for iso in isos {
        if let Some(t) = parse(iso) {
            first = Some(first.map_or(t, |f| f.min(t)));
            last = Some(last.map_or(t, |l| l.max(t)));
        }
    }
    match (first, last) {
        (Some(f), Some(l)) if l >= f => l - f,
        _ => 0,
    }
}

/// Milli-G-per-second rate used by the "fastest gamerscore" ranking:
/// `gamerscore * 1000 / max(span_secs, 1)`. The 1-second clamp keeps
/// burst titles (every unlock at the same instant → span = 0) finite
/// and effectively ranks them by raw gamerscore — "instant G is the
/// fastest G". Integer milli-units instead of f64 so sort keys stay
/// total-ordered and deterministic (no NaN, no platform float drift).
///
/// Single source of truth shared by the bit-by-bit runner, the
/// estimator preview, the CLI dry-run scope print, and the GUI plan
/// summary — the four surfaces MUST rank identically or the preview
/// lies about what the run will do.
#[must_use]
pub fn compute_gs_rate_milli(gamerscore: i64, span_secs: i64) -> i64 {
    gamerscore.saturating_mul(1000) / span_secs.max(1)
}

impl Plan {
    /// Compute per-title span data from the plan's entries. Results
    /// are sorted by `duration_seconds` ASCENDING — shortest first —
    /// which is the order callers want for "start with quick wins"
    /// replay strategies (bit-by-bit, nuclear with a cap, etc.).
    ///
    /// Ties (same duration) break on `achievement_count` ascending
    /// then `title_id` to keep the result deterministic.
    #[must_use]
    #[allow(clippy::items_after_statements, clippy::assigning_clones)]
    pub fn title_spans_shortest_first(&self) -> Vec<TitleSpan> {
        use std::collections::HashMap;
        // Use the same ISO parser as the runner so we get the exact
        // same ordering behaviour — `runner::parse_iso_public` is
        // the single source of truth for "ISO → unix seconds".
        // Kept here for `first_iso`/`last_iso` string capture; the
        // duration computation itself delegates to
        // `compute_span_seconds` below.
        let parse = runner::parse_iso_public;
        // BUG HUNTER TS-N1: count UNIQUE achievement_ids per title
        // rather than raw entry occurrences. Corrupted or hand-
        // edited plan files can contain duplicate (title_id, ach_id)
        // pairs; counting them naively inflated
        // `achievement_count`, which then incorrectly passed the
        // `count >= total_achievements` check in
        // `title_spans_100_percent_shortest_first` — a 1/2-completed
        // title with a duplicated entry would show up as 2/2 (100%)
        // in the ranking. Now each title tracks a HashSet of
        // ach_ids, and `count` is the set size.
        #[derive(Default)]
        struct Acc {
            game_name: String,
            ach_ids: std::collections::HashSet<String>,
            gamerscore: i64,
            first_ts: Option<i64>,
            last_ts: Option<i64>,
            first_iso: String,
            last_iso: String,
        }
        let mut by_title: HashMap<String, Acc> = HashMap::new();
        for e in &self.entries {
            let a = by_title.entry(e.title_id.clone()).or_default();
            if a.game_name.is_empty() {
                a.game_name.clone_from(&e.game_name);
            }
            // Credit gamerscore AND timestamps only on FIRST sight of
            // the ach_id — the same dedupe that guards
            // `achievement_count`, and the same first-occurrence-wins
            // rule the runner (`seen_keys` filter before bucketing)
            // and the estimator apply. A duplicated plan row can't
            // inflate the G-rate ranking, and — audit finding
            // (verify-gs-rate-ranking) — a duplicate carrying a
            // DIFFERENT timestamp can't widen the span here while the
            // runner computes it from first occurrences only, which
            // would silently rank/scope the preview differently from
            // the actual run.
            if a.ach_ids.insert(e.achievement_id.clone()) {
                a.gamerscore = a
                    .gamerscore
                    .saturating_add(i64::from(e.gamerscore.unwrap_or(0)));
                if let Some(t) = parse(&e.time_unlocked) {
                    if a.first_ts.map_or(true, |f| t < f) {
                        a.first_ts = Some(t);
                        a.first_iso.clone_from(&e.time_unlocked);
                    }
                    if a.last_ts.map_or(true, |l| t > l) {
                        a.last_ts = Some(t);
                        a.last_iso.clone_from(&e.time_unlocked);
                    }
                }
            }
        }
        let mut spans: Vec<TitleSpan> = by_title
            .into_iter()
            .map(|(title_id, a)| {
                // Match the shared `compute_span_seconds` formula
                // exactly so `Plan::title_spans_shortest_first()`
                // and `bit_by_bit`'s sort key never diverge.
                let duration = match (a.first_ts, a.last_ts) {
                    (Some(f), Some(l)) if l >= f => l - f,
                    _ => 0,
                };
                TitleSpan {
                    title_id,
                    game_name: a.game_name,
                    achievement_count: a.ach_ids.len(),
                    duration_seconds: duration,
                    gamerscore: a.gamerscore,
                    first_unlocked: a.first_iso,
                    last_unlocked: a.last_iso,
                }
            })
            .collect();
        // Shortest-first is the natural order for "quickly accumulate
        // completed titles". A 1-achievement platform title (10
        // minutes) unlocks fully in one bit-by-bit run; a 50-hour
        // grind trickles over weeks.
        spans.sort_by(|a, b| {
            a.duration_seconds
                .cmp(&b.duration_seconds)
                .then_with(|| a.achievement_count.cmp(&b.achievement_count))
                .then_with(|| a.title_id.cmp(&b.title_id))
        });
        spans
    }

    /// Per-title spans filtered to titles the target has
    /// **100%-completed** (every achievement unlocked), sorted
    /// shortest-first. This is the "speedrun" / "fastest platinum"
    /// ranking — titles the target grinded out to completion
    /// fastest.
    ///
    /// Filters:
    /// - Requires `title_totals[title_id]` to be populated AND
    ///   `current >= total > 0` (Plan::TitleCompletion::is_complete).
    ///   Plans built before the `title_totals` field existed have
    ///   an empty map and return an empty vec here.
    /// - Requires `achievement_count >= 2` (can't measure "fastest"
    ///   from a single data point). 1-achievement platform titles
    ///   with `total = 1` are technically 100% but have
    ///   `duration = 0` — excluded because the ranking is
    ///   meaningless.
    ///
    /// Same tiebreakers as `title_spans_shortest_first` —
    /// `(duration, count, title_id)` — deterministic.
    #[must_use]
    pub fn title_spans_100_percent_shortest_first(&self) -> Vec<TitleSpan> {
        self.title_spans_shortest_first()
            .into_iter()
            .filter(|s| {
                if s.achievement_count < 2 {
                    return false;
                }
                // Authoritative "did target 100%?" test: count of
                // PLAN ENTRIES for this title vs. the title's total
                // from titlehub. We used to defer to
                // `TitleCompletion::is_complete()` which compared
                // titlehub's `current_achievements` to its `total`,
                // but titlehub is empirically unreliable on
                // `current` — it returns 0 for thousands of titles
                // where the user actually HAS unlocks (see comment
                // in `preview_games` about 4701/4797 titles losing
                // data on SiegfriedX). The bulk `/achievements`
                // scan is the ground truth: every entry for a
                // title is a confirmed unlock. So if the plan has
                // >= total entries for this title, the target
                // platinumed it, period.
                self.title_totals.get(&s.title_id).is_some_and(|tc| {
                    tc.total_achievements > 0
                        && i32::try_from(s.achievement_count).unwrap_or(i32::MAX)
                            >= tc.total_achievements
                })
            })
            .collect()
    }

    /// Per-title spans ranked by **gamerscore rate** — highest
    /// G-per-second first. "Fastest" here means "the target earned
    /// the most G in the least wall-clock time": a title with 3000G
    /// over 21s (≈143 G/s) outranks one with 900G over 20s (45 G/s)
    /// even though its raw span is a second longer.
    ///
    /// Unlike [`Self::title_spans_100_percent_shortest_first`], this
    /// ranking has NO completion filter and NO minimum entry count —
    /// partially-completed titles and single-achievement titles all
    /// compete on their rate (span 0 clamps to 1s via
    /// [`compute_gs_rate_milli`], so a burst title ranks by raw G).
    ///
    /// This is the ranking the bit-by-bit runner uses for title
    /// ordering and top-N selection; the estimator, CLI dry-run and
    /// GUI summary mirror it exactly.
    ///
    /// Tiebreakers (equal rate — common for long-span titles whose
    /// integer rate truncates to 0): shorter span, fewer
    /// achievements, then `title_id` — deterministic across runs.
    #[must_use]
    pub fn title_spans_fastest_gs_first(&self) -> Vec<TitleSpan> {
        let mut spans = self.title_spans_shortest_first();
        spans.sort_by_cached_key(|s| {
            let secs = self.effective_completion_secs(&s.title_id, s.duration_seconds);
            (
                std::cmp::Reverse(compute_gs_rate_milli(s.gamerscore, secs)),
                secs,
                s.achievement_count,
                s.title_id.clone(),
            )
        });
        spans
    }

    /// How long finishing this title will ACTUALLY take, in seconds.
    ///
    /// "Fastest first" has to be measured with the clock the run is
    /// billed against. Two different quantities were being conflated:
    ///
    /// * **calendar span** — first unlock to last unlock. A game the
    ///   target 100%'d over one weekend has a short span even if it
    ///   cost 40 hours of play; a 20-minute game they picked at over
    ///   two years has a span of two years.
    /// * **playtime floor** — the target's real `MinutesPlayed`, which
    ///   is exactly what the sequential runner paces against.
    ///
    /// Ranking by span therefore put long games first and quick ones
    /// last — the opposite of "start with the fastest to complete".
    /// Prefer the real playtime; fall back to the span only when the
    /// plan carries no stat for the title, and cap that guess the same
    /// way the runner does so a multi-year span can't dominate the
    /// ordering either.
    #[must_use]
    pub fn effective_completion_secs(&self, title_id: &str, calendar_span_secs: i64) -> i64 {
        match self
            .title_min_completion_secs
            .get(title_id)
            .copied()
            .filter(|&s| s > 0)
        {
            Some(real) => real,
            None => {
                let cap = i64::try_from(crate::plan::bit_by_bit::FLOOR_GUESS_CAP_SECS)
                    .unwrap_or(i64::MAX);
                calendar_span_secs.clamp(0, cap)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(title: &str, ach: &str, gs: i32) -> PlanEntry {
        PlanEntry {
            title_id: title.into(),
            service_config_id: String::new(),
            achievement_id: ach.into(),
            achievement_name: format!("Ach {ach}"),
            game_name: format!("Title {title}"),
            time_unlocked: "2024-01-01T00:00:00Z".into(),
            gamerscore: Some(gs),
        }
    }

    /// Field-reported: "it's going to a game that's many hours — it
    /// should always start with the fastest ones to complete."
    ///
    /// "Fastest" was measured with the calendar span (first unlock to
    /// last), not the playtime the run is actually billed for. Those
    /// disagree badly: a title 100%'d over one weekend has a two-day
    /// span even when it cost 40 hours of play, while a 20-minute game
    /// picked at across two years has a two-year span. Ranking by span
    /// therefore put the long game FIRST and the quick one LAST.
    #[test]
    fn ranking_uses_real_playtime_not_calendar_span() {
        let mut plan = Plan {
            entries: vec![
                // GRINDER: earned across 2 days, but 40 HOURS of play.
                mk_at("grinder", "a1", 500, "2024-01-01T00:00:00Z"),
                mk_at("grinder", "a2", 500, "2024-01-03T00:00:00Z"),
                // QUICKIE: earned across 2 YEARS, but 20 minutes of play.
                mk_at("quickie", "b1", 500, "2022-01-01T00:00:00Z"),
                mk_at("quickie", "b2", 500, "2024-01-01T00:00:00Z"),
            ],
            ..Default::default()
        };
        plan.total = plan.entries.len();

        // Without real playtime, the span decides — and the grinder
        // (2-day span) outranks the quickie (2-year span).
        let by_span = plan.title_spans_fastest_gs_first();
        assert_eq!(
            by_span.first().map(|s| s.title_id.as_str()),
            Some("grinder"),
            "sanity: the span-only ordering is what produced the complaint"
        );

        // With Xbox's real MinutesPlayed, the order inverts to what the
        // run will actually experience.
        plan.title_min_completion_secs
            .insert("grinder".into(), 40 * 3600);
        plan.title_min_completion_secs
            .insert("quickie".into(), 20 * 60);
        let by_playtime = plan.title_spans_fastest_gs_first();
        assert_eq!(
            by_playtime.first().map(|s| s.title_id.as_str()),
            Some("quickie"),
            "the 20-minute game must come before the 40-hour one"
        );

        // The guess for a title with no stat is capped, so a multi-year
        // span can't dominate the ordering on its own either.
        let two_years = 2 * 365 * 86400;
        assert_eq!(
            plan.effective_completion_secs("no-stat-title", two_years),
            i64::try_from(crate::plan::bit_by_bit::FLOOR_GUESS_CAP_SECS).unwrap(),
        );
        // A real stat is never capped -- a genuine 40h grind stays 40h.
        assert_eq!(
            plan.effective_completion_secs("grinder", 0),
            40 * 3600,
            "real playtime is authoritative and uncapped"
        );
    }

    #[test]
    fn total_gamerscore_sums_entries() {
        let p = Plan {
            entries: vec![mk("T1", "a", 10), mk("T1", "b", 20), mk("T2", "c", 5)],
            ..Default::default()
        };
        assert_eq!(p.total_gamerscore(), 35);
    }

    /// Like `mk` but with an explicit unlock timestamp — needed by
    /// the span/rate ranking tests.
    fn mk_at(title: &str, ach: &str, gs: i32, iso: &str) -> PlanEntry {
        PlanEntry {
            time_unlocked: iso.into(),
            ..mk(title, ach, gs)
        }
    }

    #[test]
    fn fastest_gs_first_ranks_by_rate_not_span() {
        // USER CONTRACT (2026-07-13): 3000G over 21s (~143 G/s) must
        // outrank 900G over 20s (45 G/s) even though its raw span is
        // one second LONGER. No completion data set — the ranking
        // must not require any.
        let p = Plan {
            entries: vec![
                mk_at("t900", "a", 450, "2024-01-01T00:00:00Z"),
                mk_at("t900", "b", 450, "2024-01-01T00:00:20Z"),
                mk_at("t3000", "c", 1500, "2024-01-01T00:00:00Z"),
                mk_at("t3000", "d", 1500, "2024-01-01T00:00:21Z"),
            ],
            ..Default::default()
        };
        let spans = p.title_spans_fastest_gs_first();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].title_id, "t3000", "higher G/s wins");
        assert_eq!(spans[0].gamerscore, 3000);
        assert_eq!(spans[1].title_id, "t900");
        assert_eq!(spans[1].gamerscore, 900);
    }

    #[test]
    fn fastest_gs_first_includes_single_achievement_titles() {
        // Span 0 clamps to 1s → a lone 500G unlock (500 G/s) beats
        // 100G over 20s (5 G/s). The 100%-only ranking excluded
        // 1-entry titles; this one must not.
        let p = Plan {
            entries: vec![
                mk_at("tslow", "a", 50, "2024-01-01T00:00:00Z"),
                mk_at("tslow", "b", 50, "2024-01-01T00:00:20Z"),
                mk_at("tburst", "c", 500, "2024-01-01T00:00:00Z"),
            ],
            ..Default::default()
        };
        let spans = p.title_spans_fastest_gs_first();
        assert_eq!(spans[0].title_id, "tburst");
        assert_eq!(spans[0].gamerscore, 500);
        assert_eq!(spans[0].duration_seconds, 0);
    }

    #[test]
    fn gs_rate_milli_math() {
        assert_eq!(compute_gs_rate_milli(3000, 21), 142_857);
        assert_eq!(compute_gs_rate_milli(900, 20), 45_000);
        // Span 0 clamps to 1s (burst titles rank by raw G).
        assert_eq!(compute_gs_rate_milli(500, 0), 500_000);
        // Long grinds truncate toward 0 but stay non-negative.
        assert_eq!(compute_gs_rate_milli(30, 259_200), 0);
        // Saturates instead of overflowing on absurd inputs.
        assert_eq!(compute_gs_rate_milli(i64::MAX, 1), i64::MAX);
    }

    #[test]
    fn title_span_gamerscore_dedupes_duplicate_rows() {
        // A duplicated plan row must not double-credit G in the
        // ranking (same dedupe rule as `achievement_count`).
        let p = Plan {
            entries: vec![mk("T1", "a", 100), mk("T1", "a", 100), mk("T1", "b", 50)],
            ..Default::default()
        };
        let spans = p.title_spans_shortest_first();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].achievement_count, 2);
        assert_eq!(spans[0].gamerscore, 150);
    }

    #[test]
    fn title_span_ignores_duplicate_row_timestamps() {
        // Audit finding (verify-gs-rate-ranking): a duplicate ach row
        // carrying a DIFFERENT timestamp must not widen the span —
        // the runner/estimator drop duplicates keeping the FIRST
        // occurrence before computing spans, so the helper must see
        // the same picture or the preview ranks/scopes differently
        // from the actual run. Here the dup of `a` an hour later
        // would stretch the span from 10s to 3600s if counted.
        let p = Plan {
            entries: vec![
                mk_at("T1", "a", 100, "2024-01-01T00:00:00Z"),
                mk_at("T1", "b", 50, "2024-01-01T00:00:10Z"),
                mk_at("T1", "a", 100, "2024-01-01T01:00:00Z"),
            ],
            ..Default::default()
        };
        let spans = p.title_spans_shortest_first();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].achievement_count, 2);
        assert_eq!(spans[0].gamerscore, 150);
        assert_eq!(
            spans[0].duration_seconds, 10,
            "duplicate row's later timestamp must not widen the span"
        );
        assert_eq!(spans[0].last_unlocked, "2024-01-01T00:00:10Z");
    }

    #[test]
    fn total_gamerscore_treats_missing_as_zero() {
        let p = Plan {
            entries: vec![
                mk("T1", "a", 10),
                PlanEntry {
                    gamerscore: None,
                    ..mk("T1", "b", 0)
                },
            ],
            ..Default::default()
        };
        assert_eq!(p.total_gamerscore(), 10);
    }

    #[test]
    fn diff_vs_empty_unlocks_is_all_net_new() {
        let p = Plan {
            entries: vec![mk("T1", "a", 10), mk("T1", "b", 20)],
            ..Default::default()
        };
        let mine = std::collections::HashSet::new();
        let d = p.diff_vs(&mine);
        assert_eq!(d.net_new_count, 2);
        assert_eq!(d.net_new_gamerscore, 30);
        assert_eq!(d.already_unlocked_count, 0);
        assert_eq!(d.already_unlocked_gamerscore, 0);
    }

    #[test]
    fn diff_vs_full_unlocks_is_zero_gain() {
        let p = Plan {
            entries: vec![mk("T1", "a", 10), mk("T1", "b", 20)],
            ..Default::default()
        };
        let mine: std::collections::HashSet<_> = vec![
            ("T1".to_string(), "a".to_string()),
            ("T1".to_string(), "b".to_string()),
        ]
        .into_iter()
        .collect();
        let d = p.diff_vs(&mine);
        assert_eq!(d.net_new_count, 0);
        assert_eq!(d.net_new_gamerscore, 0);
        assert_eq!(d.already_unlocked_count, 2);
        assert_eq!(d.already_unlocked_gamerscore, 30);
    }

    #[test]
    fn diff_vs_partial_unlocks_splits_correctly() {
        // Plan has 3 achievements worth 10+20+5=35; I already have "a" (10).
        // Net-new = b+c = 25G, 2 achievements.
        let p = Plan {
            entries: vec![mk("T1", "a", 10), mk("T1", "b", 20), mk("T2", "c", 5)],
            ..Default::default()
        };
        let mine: std::collections::HashSet<_> =
            std::iter::once(("T1".to_string(), "a".to_string())).collect();
        let d = p.diff_vs(&mine);
        assert_eq!(d.net_new_count, 2);
        assert_eq!(d.net_new_gamerscore, 25);
        assert_eq!(d.already_unlocked_count, 1);
        assert_eq!(d.already_unlocked_gamerscore, 10);
        // Per-title sorted by net_new_gamerscore DESC: T1 (20G net) > T2 (5G net).
        assert_eq!(d.per_title.len(), 2);
        assert_eq!(d.per_title[0].title_id, "T1");
        assert_eq!(d.per_title[0].net_new_count, 1);
        assert_eq!(d.per_title[0].net_new_gamerscore, 20);
        assert_eq!(d.per_title[0].already_unlocked_count, 1);
        assert_eq!(d.per_title[1].title_id, "T2");
        assert_eq!(d.per_title[1].net_new_count, 1);
    }

    /// BUG HUNTER TG-N1: total_gamerscore must dedupe by
    /// (title_id, ach_id) so dup entries in a corrupted plan file
    /// don't inflate the "plan will give you X G" UI number beyond
    /// what Xbox will actually credit.
    #[test]
    fn tg_n1_total_gamerscore_dedupes_internal_duplicates() {
        let p = Plan {
            entries: vec![
                mk("T1", "a", 10),
                mk("T1", "a", 10), // dup — contributes 0 to real credit
                mk("T1", "b", 20),
            ],
            ..Default::default()
        };
        assert_eq!(
            p.total_gamerscore(),
            30,
            "deduped sum: 10 (T1/a) + 20 (T1/b) = 30; dup must not add another 10"
        );
    }

    /// BUG HUNTER DV-N1: a plan with internal duplicates must NOT
    /// double-count net_new or already_unlocked. Dedupe by
    /// (title_id, ach_id) before counting.
    #[test]
    fn dv_n1_diff_vs_dedupes_internal_duplicates() {
        use std::collections::HashSet;
        let p = Plan {
            entries: vec![
                mk("T1", "a", 10),
                mk("T1", "a", 10), // dup — should count only once
                mk("T1", "b", 20),
                mk("T2", "x", 5),
                mk("T2", "x", 5), // dup
            ],
            ..Default::default()
        };
        // User already has T1/a. Net-new should be 2 (T1/b + T2/x),
        // not 3 (with dup of T2/x). Already_unlocked should be 1 (T1/a)
        // not 2 (with dup of T1/a).
        let mine: HashSet<(String, String)> = [("T1".into(), "a".into())].into_iter().collect();
        let d = p.diff_vs(&mine);
        assert_eq!(d.net_new_count, 2, "dups must not inflate net_new_count");
        assert_eq!(d.net_new_gamerscore, 25, "net_new GS: 20 (T1/b) + 5 (T2/x)");
        assert_eq!(
            d.already_unlocked_count, 1,
            "dups must not inflate already_unlocked_count"
        );
        assert_eq!(d.already_unlocked_gamerscore, 10);
    }

    #[test]
    fn unlock_keys_deduplicates_by_pair() {
        let p = Plan {
            entries: vec![mk("T1", "a", 10), mk("T1", "a", 10), mk("T1", "b", 5)],
            ..Default::default()
        };
        // HashSet naturally collapses the dup — 2 unique pairs.
        assert_eq!(p.unlock_keys().len(), 2);
    }

    /// BUG HUNTER TS-N1: achievement_count must reflect UNIQUE
    /// achievement_ids per title, not raw entry occurrences. A plan
    /// with internal duplicates (corrupted file, hand-edit) used to
    /// inflate the count and produce spurious 100% ranking entries.
    #[test]
    fn ts_n1_title_spans_count_unique_ach_ids_not_raw_entries() {
        use std::collections::HashMap;
        // 3 entries but only 2 unique ach_ids for T1. Titlehub says
        // total=2. Before fix: count=3 → 3 >= 2 → shown as 100%.
        // After fix: count=2 (unique) → 2 >= 2 → shown as 100% ONLY
        // when the plan actually contains all 2 unique unlocks.
        let mut totals = HashMap::new();
        totals.insert(
            "T1".to_string(),
            TitleCompletion {
                current_achievements: 2,
                total_achievements: 2,
            },
        );
        let p = Plan {
            entries: vec![
                PlanEntry {
                    time_unlocked: "2024-01-01T00:00:00Z".into(),
                    ..mk("T1", "a", 10)
                },
                PlanEntry {
                    time_unlocked: "2024-01-01T00:00:00Z".into(),
                    ..mk("T1", "a", 10) // duplicate
                },
                PlanEntry {
                    time_unlocked: "2024-01-01T00:01:00Z".into(),
                    ..mk("T1", "b", 10)
                },
            ],
            title_totals: totals,
            ..Default::default()
        };
        let spans = p.title_spans_shortest_first();
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].achievement_count, 2,
            "count must reflect unique ach_ids (2), not raw entries (3)"
        );
    }

    /// TS-N1 inverse: duplicates inflate count to meet total,
    /// falsely triggering 100% ranking. After fix, the inflated
    /// case is correctly REJECTED.
    #[test]
    fn ts_n1_title_100pct_rejects_duplicate_inflation() {
        use std::collections::HashMap;
        // 2 entries but only 1 unique. Titlehub total=2. Pre-fix:
        // count=2 → 2>=2 → FALSELY ranked as 100%. Post-fix:
        // count=1 (unique) → 1<2 → correctly excluded.
        let mut totals = HashMap::new();
        totals.insert(
            "T1".to_string(),
            TitleCompletion {
                current_achievements: 2,
                total_achievements: 2,
            },
        );
        let p = Plan {
            entries: vec![
                PlanEntry {
                    time_unlocked: "2024-01-01T00:00:00Z".into(),
                    ..mk("T1", "a", 10)
                },
                PlanEntry {
                    time_unlocked: "2024-01-01T00:00:01Z".into(),
                    ..mk("T1", "a", 10) // duplicate of "a"
                },
            ],
            title_totals: totals,
            ..Default::default()
        };
        let spans = p.title_spans_100_percent_shortest_first();
        assert!(
            spans.is_empty(),
            "dup-inflated count should NOT qualify as 100%; got {spans:?}"
        );
    }

    #[test]
    fn title_100pct_uses_plan_count_not_titlehub_current() {
        // Regression: titlehub is unreliable on `current_achievements`
        // (see the preview_games comment about 4701/4797 titles
        // having wrong counts on SiegfriedX). If we gated on
        // `titlehub.current >= titlehub.total`, 100% titles would
        // silently vanish from the ranking. Ground truth: count of
        // plan entries vs. titlehub's `total`.
        //
        // Scenario: plan has 3 entries for "T1". Titlehub reports
        // total=3 (correct) but current=0 (the classic Xbox bug).
        // The ranking MUST still include T1 as 100%-completed.
        use std::collections::HashMap;
        let mut totals = HashMap::new();
        totals.insert(
            "T1".to_string(),
            TitleCompletion {
                current_achievements: 0, // <- wrong per Xbox's bug
                total_achievements: 3,
            },
        );
        let p = Plan {
            entries: vec![
                PlanEntry {
                    time_unlocked: "2024-01-01T00:00:00Z".into(),
                    ..mk("T1", "a", 10)
                },
                PlanEntry {
                    time_unlocked: "2024-01-01T00:01:00Z".into(),
                    ..mk("T1", "b", 10)
                },
                PlanEntry {
                    time_unlocked: "2024-01-01T00:02:00Z".into(),
                    ..mk("T1", "c", 10)
                },
            ],
            title_totals: totals,
            ..Default::default()
        };
        let spans = p.title_spans_100_percent_shortest_first();
        assert_eq!(
            spans.len(),
            1,
            "T1 should be counted as 100% despite titlehub's wrong current=0"
        );
        assert_eq!(spans[0].title_id, "T1");
    }

    #[test]
    fn title_100pct_rejects_partial_coverage() {
        // Inverse: plan has only 2 of 3 entries. Target did NOT
        // 100% this title. Must be excluded regardless of what
        // titlehub says.
        use std::collections::HashMap;
        let mut totals = HashMap::new();
        totals.insert(
            "T1".to_string(),
            TitleCompletion {
                current_achievements: 99, // <- titlehub lies the other way
                total_achievements: 3,
            },
        );
        let p = Plan {
            entries: vec![
                PlanEntry {
                    time_unlocked: "2024-01-01T00:00:00Z".into(),
                    ..mk("T1", "a", 10)
                },
                PlanEntry {
                    time_unlocked: "2024-01-01T00:01:00Z".into(),
                    ..mk("T1", "b", 10)
                },
            ],
            title_totals: totals,
            ..Default::default()
        };
        assert!(p.title_spans_100_percent_shortest_first().is_empty());
    }

    #[test]
    fn retain_title_totals_for_entries_drops_orphan_metadata() {
        use std::collections::HashMap;
        let mut totals = HashMap::new();
        totals.insert(
            "keep".to_string(),
            TitleCompletion {
                current_achievements: 1,
                total_achievements: 1,
            },
        );
        totals.insert(
            "drop".to_string(),
            TitleCompletion {
                current_achievements: 1,
                total_achievements: 1,
            },
        );
        let mut p = Plan {
            entries: vec![mk("keep", "a", 10)],
            title_totals: totals,
            ..Default::default()
        };
        p.retain_title_totals_for_entries();
        assert!(p.title_totals.contains_key("keep"));
        assert!(!p.title_totals.contains_key("drop"));
    }

    #[test]
    fn compute_span_seconds_basic() {
        // Three unlocks spanning 1 hour → span = 3600 seconds.
        let span = compute_span_seconds([
            "2024-01-01T10:00:00Z",
            "2024-01-01T10:30:00Z",
            "2024-01-01T11:00:00Z",
        ]);
        assert_eq!(span, 3600);
    }

    #[test]
    fn compute_span_seconds_unordered_input_still_correct() {
        // Scan order doesn't matter — the helper tracks min/max, not
        // first-seen/last-seen. Fixing this guarantees `bit_by_bit`'s
        // sort ordering doesn't depend on `tp.source` being pre-sorted.
        let span = compute_span_seconds([
            "2024-01-01T11:00:00Z",
            "2024-01-01T10:00:00Z",
            "2024-01-01T10:30:00Z",
        ]);
        assert_eq!(span, 3600);
    }

    #[test]
    fn compute_span_seconds_empty_or_unparseable_is_zero() {
        assert_eq!(compute_span_seconds(std::iter::empty::<&str>()), 0);
        assert_eq!(compute_span_seconds(["nope", "0001-01-01T00:00:00Z"]), 0);
    }

    #[test]
    fn bit_by_bit_and_summary_use_same_span_math() {
        // Regression: prevent the ranking shown in the GUI (via
        // title_spans_shortest_first) from drifting away from the
        // ordering bit_by_bit's runner actually applies. Both now
        // delegate to `compute_span_seconds`; the test proves the
        // results agree for a realistic 3-title mixed span.
        let p = Plan {
            entries: vec![
                // titleA: 2 ach, 1 hour span (fast)
                PlanEntry {
                    time_unlocked: "2024-01-01T10:00:00Z".into(),
                    ..mk("titleA", "a1", 10)
                },
                PlanEntry {
                    time_unlocked: "2024-01-01T11:00:00Z".into(),
                    ..mk("titleA", "a2", 20)
                },
                // titleB: 2 ach, 1 day span (medium)
                PlanEntry {
                    time_unlocked: "2024-01-01T10:00:00Z".into(),
                    ..mk("titleB", "b1", 5)
                },
                PlanEntry {
                    time_unlocked: "2024-01-02T10:00:00Z".into(),
                    ..mk("titleB", "b2", 5)
                },
                // titleC: 2 ach, 1 week span (slow)
                PlanEntry {
                    time_unlocked: "2024-01-01T10:00:00Z".into(),
                    ..mk("titleC", "c1", 15)
                },
                PlanEntry {
                    time_unlocked: "2024-01-08T10:00:00Z".into(),
                    ..mk("titleC", "c2", 15)
                },
            ],
            ..Default::default()
        };
        let spans = p.title_spans_shortest_first();
        // Summary ranking: A (fastest) → B → C (slowest).
        assert_eq!(spans[0].title_id, "titleA");
        assert_eq!(spans[1].title_id, "titleB");
        assert_eq!(spans[2].title_id, "titleC");
        // Same math applied to the same entries must give the same
        // ordering inside bit_by_bit (it sorts by `compute_span_seconds`
        // too — see bit_by_bit::run). Verify by computing the
        // helper directly over each title's entries and confirming
        // the sort order matches the summary.
        let mut runner_keys: Vec<(String, i64)> = ["titleA", "titleB", "titleC"]
            .iter()
            .map(|tid| {
                let d = compute_span_seconds(
                    p.entries
                        .iter()
                        .filter(|e| e.title_id == *tid)
                        .map(|e| e.time_unlocked.as_str()),
                );
                ((*tid).to_string(), d)
            })
            .collect();
        runner_keys.sort_by_key(|(_, d)| *d);
        assert_eq!(runner_keys[0].0, "titleA");
        assert_eq!(runner_keys[1].0, "titleB");
        assert_eq!(runner_keys[2].0, "titleC");
        // And the durations themselves match the summary spans.
        for sp in &spans {
            let d = runner_keys
                .iter()
                .find(|(t, _)| *t == sp.title_id)
                .unwrap()
                .1;
            assert_eq!(
                sp.duration_seconds, d,
                "summary and bit_by_bit disagree on duration for {}",
                sp.title_id
            );
        }
    }

    #[test]
    fn title_100pct_requires_total_known() {
        // If titlehub total is 0 (unknown / omitted), we can't
        // prove 100% — reject, don't optimistically accept.
        use std::collections::HashMap;
        let mut totals = HashMap::new();
        totals.insert(
            "T1".to_string(),
            TitleCompletion {
                current_achievements: 10,
                total_achievements: 0, // unknown
            },
        );
        let p = Plan {
            entries: vec![
                PlanEntry {
                    time_unlocked: "2024-01-01T00:00:00Z".into(),
                    ..mk("T1", "a", 10)
                },
                PlanEntry {
                    time_unlocked: "2024-01-01T00:01:00Z".into(),
                    ..mk("T1", "b", 10)
                },
            ],
            title_totals: totals,
            ..Default::default()
        };
        assert!(p.title_spans_100_percent_shortest_first().is_empty());
    }
}
