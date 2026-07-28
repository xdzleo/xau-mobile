//! Build a [`Plan`](super::Plan) by walking the target profile's games and
//! their per-title achievement lists.
//!
//! We fan out concurrent `get_achievements_for_title` calls using tokio's
//! buffered stream so large libraries (SiegfriedX has ~4000 games) finish in
//! minutes rather than hours — while still respecting a soft concurrency cap
//! to avoid Xbox Live rate-limiting us.

// Fixed-range date arithmetic. All casts are sound for 1970..=9999.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::many_single_char_names
)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;

/// Drop-reason tallies for the parallel filter pass in [`build_plan`].
///
/// Hoisted to module scope (not defined inside the function) because the
/// filter runs on a rayon work-stealing pool behind `spawn_blocking` and
/// needs a concrete `Send + Sync + 'static` container the worker closures
/// can share via `Arc`. Individual counters are `AtomicUsize` with
/// `Relaxed` ordering — the only reader runs AFTER the parallel iter
/// has fully drained, so we never need the stricter happens-before
/// guarantees of `AcqRel`.
#[derive(Default)]
struct DropCounters {
    non_replayable: AtomicUsize,
    include_filter: AtomicUsize,
    skip_filter: AtomicUsize,
    min_unlocked_filter: AtomicUsize,
    missing_timestamp: AtomicUsize,
    no_title_assoc: AtomicUsize,
    // Unlock-safety gates. Reported separately so the GUI can show
    // the user exactly how many revoked / time-window entries got
    // filtered during the scan.
    safety_revoked: AtomicUsize,
    safety_time_expired: AtomicUsize,
    safety_time_not_yet_open: AtomicUsize,
}

/// Shorthand for the progress / checkpoint callback shape. Keeps the type
/// below from tripping `clippy::type_complexity`.
pub type ProgressCallback = Arc<dyn Fn(usize, usize) + Send + Sync>;
pub type CheckpointCallback = Arc<dyn Fn(&[PlanEntry]) + Send + Sync>;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::xbox::XboxClient;

use super::{Plan, PlanEntry};

/// Tuning knobs for [`build_plan`].
#[derive(Clone)]
pub struct BuildOptions {
    /// **Deprecated** — legacy serial-per-title fetch knob. After the
    /// bulk-scan refactor there is no per-title fan-out left in
    /// `build_plan`; the bulk endpoint handles parallelism
    /// internally with hardcoded page tuning, and the per-title
    /// totals backfill uses a fixed semaphore of 8. This field is
    /// still public for ABI compatibility with callers that set it,
    /// but the value is ignored.
    #[deprecated(note = "ignored after the bulk-scan refactor — parallelism is \
                now governed by the bulk endpoint's internal page \
                tuning, not this knob")]
    pub concurrency: usize,
    /// Skip titles with fewer than this many unlocked achievements. Default 1
    /// (include every started title).
    pub min_unlocked: i32,
    /// If `true`, only include entries whose `time_unlocked` is populated.
    /// Default `true` because entries without a timestamp can't be ordered.
    pub require_timestamp: bool,
    /// If `true`, skip titles that only appear on Xbox 360 — those can't be
    /// unlocked through the modern `achievements.xboxlive.com/.../update`
    /// endpoint anyway, so scanning them wastes round-trips. Default `true`.
    pub skip_xbox_360: bool,
    /// If `Some`, only titles whose titleId is in this set are scanned. This
    /// lets the GUI let the user pick specific games ("just these 5 from
    /// SiegfriedX's library") without touching the rest. `None` means
    /// "every game the target profile has played".
    pub include_title_ids: Option<HashSet<String>>,
    /// Titles we've already scanned in a previous attempt — don't re-fetch
    /// them. Used for the "resume after timeout" flow: caller passes the
    /// title_ids present in an existing partial plan, and `build_plan`
    /// only scans the ones that are missing. The caller is responsible
    /// for merging the resulting entries with the previously-saved ones.
    pub skip_title_ids: Option<HashSet<String>>,
    /// Stop after this many achievement entries have been collected (0 = no
    /// cap). Applied post-sort, so you still get the OLDEST `max_entries`
    /// entries — useful when the caller also wants to cap the replay size at
    /// build time rather than scan the whole library first.
    pub max_entries: usize,
    /// Called with `(done, total)` every time a title is processed. Use this
    /// to drive a progress bar in the GUI.
    pub progress: Option<ProgressCallback>,
    /// Called **once**, after the bulk scan and parallel filter have
    /// produced the full entry list, with the final sorted snapshot.
    /// Use this to write a plan file to disk so the next launch can
    /// `--resume` from a completed scan.
    ///
    /// **Not** a periodic mid-scan checkpoint: the bulk endpoint is
    /// atomic (one pipelined fetch + one parallel-filter pass), so
    /// there's no intermediate state we can meaningfully snapshot.
    /// A crash before completion leaves nothing on disk. If periodic
    /// checkpoints are needed, the scan architecture has to be
    /// refactored to yield per-page chunks.
    pub checkpoint: Option<CheckpointCallback>,
    /// **Deprecated** — retained as a public field so external callers
    /// that set it still compile, but the value is ignored. The
    /// checkpoint callback now fires exactly once at the end of a
    /// successful scan (see the `checkpoint` field docs for why).
    #[deprecated(note = "the bulk scan is atomic; checkpoint fires once at the end. \
                This field is ignored.")]
    pub checkpoint_every: usize,
    /// Unlock-safety policy applied at plan-build time. Defaults to
    /// `SafetyPolicy::strict()` — every revoked / time-window-expired /
    /// time-window-not-yet-open achievement is filtered out before it
    /// ever reaches the plan. See `plan::safety` for the full rationale.
    pub safety: crate::plan::safety::SafetyPolicy,
    /// BRUTAL SCAN FIX C1.2/1.3: if Some, the bulk scan phase writes
    /// a persistent checkpoint every 5 pages to this path (typically
    /// `{out}.scan_cache`). On retry, the cache is loaded and
    /// already-fetched offsets are skipped. The cache is deleted
    /// automatically on successful scan completion. MEGA profiles
    /// (126k+ achievements) that consistently stall mid-scan converge
    /// reliably when this is set — typically 2-4 retry runs.
    pub scan_cache_path: Option<std::path::PathBuf>,
    /// BRUTAL SCAN FIX C2.2: when Some, the bulk scan page fetches
    /// round-robin through this pool of XboxClients. Xbox rate limit
    /// is PER-SCANNER-XUID; N validated tokens = N independent rate
    /// buckets = up to N× effective throughput. Pool is typically
    /// built from the TokenPool's validated entries.
    pub scan_pool: Option<Arc<Vec<XboxClient>>>,
}

impl std::fmt::Debug for BuildOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildOptions")
            .field("concurrency", &{
                #[allow(deprecated)]
                {
                    self.concurrency
                }
            })
            .field("min_unlocked", &self.min_unlocked)
            .field("require_timestamp", &self.require_timestamp)
            .field("skip_xbox_360", &self.skip_xbox_360)
            .field(
                "include_title_ids",
                &self.include_title_ids.as_ref().map(HashSet::len),
            )
            .field(
                "skip_title_ids",
                &self.skip_title_ids.as_ref().map(HashSet::len),
            )
            .field("max_entries", &self.max_entries)
            .field("progress", &self.progress.as_ref().map(|_| "<fn>"))
            .field("checkpoint", &self.checkpoint.as_ref().map(|_| "<fn>"))
            // Deprecated — Debug-print still reads it to stay
            // backward-compatible with existing log output.
            .field("checkpoint_every", &{
                #[allow(deprecated)]
                {
                    self.checkpoint_every
                }
            })
            .field("safety", &self.safety)
            .finish()
    }
}

impl Default for BuildOptions {
    fn default() -> Self {
        #[allow(deprecated)]
        Self {
            // 32 is the sweet spot on achievements.xboxlive.com based on
            // burn-in tests: higher than 32 starts drawing 429s, lower than
            // 16 wastes CPU + bandwidth. The HTTP layer has 429 backoff,
            // so transient throttles don't break the scan — they just slow
            // that one title.
            concurrency: 32,
            min_unlocked: 1,
            require_timestamp: true,
            // Xbox 360 titles can't be unlocked via the modern Xbox Live
            // `/achievements/update` endpoint we use, so scanning them is
            // pure wasted work.
            skip_xbox_360: true,
            include_title_ids: None,
            skip_title_ids: None,
            max_entries: 0,
            progress: None,
            checkpoint: None,
            // Retained for ABI compat; ignored — see docstring.
            checkpoint_every: 50,
            // Default strict — revoked + expired + pre-release are all
            // filtered at plan build. The caller can opt into
            // `danger_allow_unsafe` via the CLI `--allow-unobtainable`
            // flag or its GUI equivalent.
            safety: crate::plan::safety::SafetyPolicy::strict(),
            // No scan cache by default — CLI (`plan-from`) and IPC
            // handler (`plans.build_to_folder`) opt in explicitly
            // when the output path is known.
            scan_cache_path: None,
            // No pool by default — CLI / IPC opt in explicitly
            // when TokenPool has ≥2 validated entries.
            scan_pool: None,
        }
    }
}

/// One row in the games-preview response. Just enough for a UI picker —
/// no per-achievement data yet (that's what `build_plan` does).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GamePreview {
    pub title_id: String,
    pub name: String,
    pub current_achievements: i32,
    pub total_achievements: i32,
    pub current_gamerscore: i32,
    pub total_gamerscore: i32,
    pub last_time_played: Option<String>,
    /// Raw device list from Xbox Live. Possible values seen in the wild
    /// (case-sensitive): "PC", "Win32", "XboxOne", "XboxSeries", "Xbox360",
    /// "Cloud", "Mobile", "iOS", "Android", "Kindle Fire", "Nintendo Switch".
    /// The UI uses this to render a badge per row.
    #[serde(default)]
    pub devices: Vec<String>,
}

/// Aggregated stats over a profile's entire games library.
///
/// Computed from a single `get_games_list` response — the per-title
/// achievement counts are already included in that payload. No extra
/// HTTP calls needed.
///
/// Used by the GUI's "profile overview" card ("29.488 / 2.990.400
/// achievements unlocked") and the CLI's extended `profile` output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileTotals {
    /// Every title the profile has ever launched (replayable or not).
    pub total_titles: usize,
    /// Subset of `total_titles` that we can actually unlock via the
    /// modern `/achievements/update` endpoint. Filters out Xbox
    /// 360-only / mobile-only / Switch-only games.
    pub replayable_titles: usize,
    /// Titles already at 100% (`current >= total && total > 0`).
    pub completed_titles: usize,
    /// Achievements the profile has unlocked, summed across all titles.
    pub current_achievements: i64,
    /// Achievements that exist to unlock (Xbox's authoritative count,
    /// = "max possible"), summed across all titles. Note: Xbox does
    /// NOT count time-limited achievements that expired before the
    /// user started the game, so this is an honest "still unlockable"
    /// ceiling.
    pub total_achievements: i64,
    /// Gamerscore earned so far, summed across all titles.
    pub current_gamerscore: i64,
    /// Max possible gamerscore across all titles the profile owns.
    pub total_gamerscore: i64,
}

impl ProfileTotals {
    /// Achievements still to unlock (`total - current`). Saturates at
    /// zero — some Xbox payloads return `current > total` for edge
    /// cases (delisted achievements still credited).
    #[must_use]
    pub fn pending_achievements(&self) -> i64 {
        (self.total_achievements - self.current_achievements).max(0)
    }

    /// Gamerscore still to earn (`total - current`), saturating at zero.
    #[must_use]
    pub fn pending_gamerscore(&self) -> i64 {
        (self.total_gamerscore - self.current_gamerscore).max(0)
    }

    /// Completion ratio as a fraction in `[0.0, 1.0]`, based on
    /// achievement counts. Returns 0.0 when `total_achievements == 0`
    /// (fresh profile, no games launched).
    ///
    /// The clamp is load-bearing, not defensive dressing: the doc above
    /// promised `[0.0, 1.0]` while the arithmetic could exceed it. Xbox
    /// under-reports per-title totals for titles credited by a replay
    /// (see `compute_profile_totals`), and a caller that trusted the
    /// documented range rendered the result as a bar width — producing
    /// `width: 1516.42%` and a fill that painted outside its container.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn completion_ratio(&self) -> f64 {
        if self.total_achievements <= 0 {
            return 0.0;
        }
        ((self.current_achievements as f64) / (self.total_achievements as f64)).clamp(0.0, 1.0)
    }
}

/// Compute aggregate stats over every game on `target_xuid`'s profile.
///
/// Makes TWO HTTP calls in parallel:
/// 1. `titlehub/titleHistory` → per-title achievement/gamerscore breakdown
/// 2. `peopleHub/users/.../profile` → authoritative TOTAL gamerscore
///
/// # Why both sources
///
/// The titlehub games list only shows titles the profile has **actually
/// launched** (Xbox tracks presence/heartbeat server-side). Achievements
/// unlocked via a replay don't register the title as "played", so a
/// profile that just finished a bulk replay of 200 games still shows
/// 2 titles in titlehub.
///
/// The peopleHub profile endpoint, however, tracks the AUTHORITATIVE
/// gamerscore across all achievements the server has on record. So we
/// use it as the truth source for `current_gamerscore`.
///
/// What we STILL can't know without scanning every title: the total
/// *pending* achievement count across all games the profile has ever
/// touched. Xbox doesn't expose a single number for that. The titlehub
/// sum is a lower bound — if you replay unlocked achievements for
/// games you never launched, those titles are absent.
pub async fn compute_profile_totals(
    client: &XboxClient,
    target_xuid: &str,
) -> Result<ProfileTotals> {
    if target_xuid.trim().is_empty() {
        return Err(Error::invalid_argument("empty target_xuid"));
    }

    // Fire both requests concurrently — they hit different Xbox
    // subdomains so they don't share rate-limit budgets.
    let (games_res, profile_res) = tokio::join!(
        client.get_games_list(target_xuid),
        client.get_profile(target_xuid),
    );
    let games = games_res?;

    let mut totals = ProfileTotals::default();
    for t in &games.titles {
        totals.total_titles += 1;
        if is_title_replayable(&t.devices) {
            totals.replayable_titles += 1;
        }
        let ach = t.achievement.clone().unwrap_or_default();
        // PER-TITLE CLAMP. titlehub reports a stale (often ZERO)
        // `total_achievements` for titles the profile never launched —
        // achievements credited by a replay don't register the title as
        // played, but the unlock count still comes back. Summing raw
        // then produces `current > total` across the library, which the
        // UI rendered as "1516,4% complete", a progress bar that
        // escaped its card, and "0/2034 games at 100%" (the completion
        // test below can never pass when the total is 0) — all while
        // the gamerscore row beside it correctly said 100%.
        //
        // Treat the count actually held as the floor for the total,
        // exactly as the authoritative-gamerscore branch below already
        // does for `total_gamerscore`. This keeps `current` truthful and
        // makes the total what this function's own docs already call it:
        // a LOWER BOUND.
        let ach_total = ach.total_achievements.max(ach.current_achievements);
        totals.current_achievements += i64::from(ach.current_achievements);
        totals.total_achievements += i64::from(ach_total);
        totals.current_gamerscore += i64::from(ach.current_gamerscore);
        totals.total_gamerscore += i64::from(ach.total_gamerscore);
        if ach_total > 0 && ach.current_achievements >= ach_total {
            totals.completed_titles += 1;
        }
    }

    // Overwrite `current_gamerscore` with the authoritative profile-
    // endpoint value when it's MORE than what the games list sum gave
    // us (after a bulk replay, the profile endpoint updates faster
    // than titlehub). Ignore profile-endpoint errors — keep titlehub
    // sum as fallback.
    if let Ok(profile) = profile_res {
        if let Some(person) = profile.people.first() {
            if let Some(gs) = person
                .gamer_score
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
            {
                if gs > totals.current_gamerscore {
                    totals.current_gamerscore = gs;
                    // If authoritative gamerscore exceeds titlehub
                    // max, raise titlehub max too — otherwise the
                    // progress bar would show >100% (nonsensical UX).
                    if gs > totals.total_gamerscore {
                        totals.total_gamerscore = gs;
                    }
                }
            }
        }
    }

    Ok(totals)
}

/// A title aggregated from the bulk achievements scan. Carries enough
/// info for the nuclear runner and the GUI games list to display + act
/// without needing a follow-up per-title GET.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnumeratedTitle {
    pub title_id: String,
    pub name: String,
    /// First non-empty SCID seen for this title in the bulk response.
    /// Populated so the runner can skip `resolve_service_config_id`.
    pub service_config_id: String,
    pub current_achievements: i32,
    pub total_achievements: i32,
    pub current_gamerscore: i32,
    pub total_gamerscore: i32,
}

impl EnumeratedTitle {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.total_achievements > 0 && self.current_achievements >= self.total_achievements
    }
    #[must_use]
    pub fn pending_achievements(&self) -> i32 {
        (self.total_achievements - self.current_achievements).max(0)
    }
}

/// Enumerate every title the caller's profile has achievements for,
/// using the bulk achievements endpoint (`unlockedOnly=false`).
///
/// This is the **only** complete enumeration Xbox Live exposes: the
/// `titlehub/titleHistory` games list returns only titles with
/// gameplay presence (user launched the app at least once), so a
/// profile that unlocked achievements via replay/nuclear shows none
/// of those titles — even though Xbox Live definitely tracks the
/// achievements server-side.
///
/// The bulk endpoint has no such filter: it returns every achievement
/// on the profile, regardless of presence, paginated at 1000 per page.
/// We aggregate per-title counts in one pass.
///
/// Cost: ~N/1000 HTTP requests (N = total achievements on profile).
/// Roughly 10 seconds for a 10k-achievement profile, 60 seconds for a
/// 126k-achievement profile. Same budget as `compute_profile_deep_totals`.
///
/// Returned titles are sorted by `total_achievements DESC` so the
/// biggest libraries show first — useful in UIs that render a
/// scrollable list (user expects "most content" on top).
pub async fn enumerate_titles_from_bulk_scan(
    client: &XboxClient,
    target_xuid: &str,
) -> Result<Vec<EnumeratedTitle>> {
    if target_xuid.trim().is_empty() {
        return Err(Error::invalid_argument("empty target_xuid"));
    }
    let all = client
        .get_all_unlocked_achievements(target_xuid, false)
        .await?;
    let mut by_title: HashMap<String, EnumeratedTitle> = HashMap::new();
    for a in &all {
        let Some(assoc) = a.title_associations.first() else {
            continue;
        };
        let Some(title_id) = assoc.id.clone() else {
            continue;
        };
        let entry = by_title
            .entry(title_id.clone())
            .or_insert_with(|| EnumeratedTitle {
                title_id,
                ..Default::default()
            });
        // Fill the display fields once.
        if entry.name.is_empty() {
            if let Some(n) = assoc.name.as_deref() {
                entry.name = n.to_string();
            }
        }
        if entry.service_config_id.is_empty() && !a.service_config_id.is_empty() {
            entry.service_config_id.clone_from(&a.service_config_id);
        }
        entry.total_achievements = entry.total_achievements.saturating_add(1);
        let achieved = a.progress_state.eq_ignore_ascii_case("Achieved");
        if achieved {
            entry.current_achievements = entry.current_achievements.saturating_add(1);
        }
        // Gamerscore from rewards array.
        if let Some(gs) = a
            .rewards
            .iter()
            .find(|r| r.ty.as_deref() == Some("Gamerscore"))
            .and_then(|r| r.value.as_deref())
            .and_then(|v| v.parse::<i32>().ok())
        {
            entry.total_gamerscore = entry.total_gamerscore.saturating_add(gs);
            if achieved {
                entry.current_gamerscore = entry.current_gamerscore.saturating_add(gs);
            }
        }
    }
    let mut titles: Vec<EnumeratedTitle> = by_title.into_values().collect();
    // Sort: most total achievements first → caller can paginate with
    // the heaviest titles on top.
    titles.sort_unstable_by(|a, b| {
        b.total_achievements
            .cmp(&a.total_achievements)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(titles)
}

/// Deep aggregate stats — scans every achievement on the profile via
/// the bulk achievements endpoint (`unlockedOnly=false`) to get the
/// REAL catalogue-wide total. Much more expensive than
/// [`compute_profile_totals`] because it pages through all achievements
/// server-side (can be 100+ requests for a big library), but gives an
/// honest "X unlocked / Y total" counter that includes titles without
/// presence.
///
/// # How it compares to `compute_profile_totals`
///
/// | Metric | `compute_profile_totals` | `compute_profile_deep_totals` |
/// |---|---|---|
/// | Source | titlehub games list + profile | bulk achievements scan + profile |
/// | Requests | 2 | 100+ (depends on profile size) |
/// | Wall-clock | ~1s | 10-60s |
/// | Accuracy on `total_achievements` | only titles with presence | all achievements on profile |
/// | Accuracy on `current_gamerscore` | authoritative (profile endpoint) | authoritative |
///
/// Use `compute_profile_totals` for fast refresh. Use
/// `compute_profile_deep_totals` when the user explicitly clicks
/// "Deep refresh" (GUI) or passes `--deep` (CLI).
pub async fn compute_profile_deep_totals(
    client: &XboxClient,
    target_xuid: &str,
) -> Result<ProfileTotals> {
    compute_profile_deep_totals_pooled::<fn(usize, Option<i64>)>(
        client,
        target_xuid,
        None,
        None,
        None,
    )
    .await
}

/// Pool-aware + resumable variant of `compute_profile_deep_totals`.
///
/// BUG HUNTER M-gui-deeptotals: the original `compute_profile_deep_totals`
/// took a single `XboxClient` and burned its rate-limit budget alone.
/// For OWN-profile deep refresh on a power user with N validated
/// pool tokens, that meant N-1 sibling tokens sat idle while the
/// scan crawled offset-by-offset on a single bucket. This pooled
/// variant accepts the same `scan_pool` shape the bulk plan builder
/// uses, multiplying effective throughput by the number of distinct
/// xuids in the pool. With 5 tokens, a SiegfriedX-sized profile
/// drops from ~30s to ~6-8s.
///
/// Also accepts an optional `scan_cache_path` so a deep-totals
/// refresh that gets killed mid-flight (laptop close, network
/// blip, app restart) resumes from the saved offsets on the next
/// click instead of re-fetching every page from zero.
pub async fn compute_profile_deep_totals_pooled<F>(
    client: &XboxClient,
    target_xuid: &str,
    scan_cache_path: Option<std::path::PathBuf>,
    scan_pool: Option<Arc<Vec<XboxClient>>>,
    progress: Option<F>,
) -> Result<ProfileTotals>
where
    F: FnMut(usize, Option<i64>) + Send + 'static,
{
    if target_xuid.trim().is_empty() {
        return Err(Error::invalid_argument("empty target_xuid"));
    }

    // Fetch ALL achievements (locked + unlocked) via the pooled scan
    // (so N tokens × per-token budget = N× effective throughput) and
    // the authoritative profile/gamerscore in parallel. The profile
    // GET shares a tiny budget with the scan but it's a single
    // request — fire-and-forget, rate-limit impact is rounding error.
    //
    // Progress callback wires through to the bulk-pages page-walker,
    // emitting (done, total) so the GUI's home-page Library Progress
    // card can show a live progress bar instead of a static spinner
    // for the 6+ min a 2M-G profile takes.
    let (all_res, profile_res) = tokio::join!(
        client.get_all_unlocked_achievements_pooled(
            target_xuid,
            false,
            progress,
            scan_cache_path,
            scan_pool,
        ),
        client.get_profile(target_xuid),
    );
    let all = all_res?;

    // Tally across title associations — the bulk response carries the
    // title each achievement belongs to in the first `title_associations`
    // entry.
    let mut current = 0_i64;
    let total = i64::try_from(all.len()).unwrap_or(i64::MAX);
    let mut current_gs = 0_i64;
    let mut total_gs = 0_i64;
    let mut titles_seen: HashSet<String> = HashSet::new();
    let mut titles_replayable: HashSet<String> = HashSet::new();
    // Per-title completion tally so we can count `completed_titles`
    // without a second scan.
    let mut per_title_counts: HashMap<String, (i64, i64)> = HashMap::new(); // title_id → (current, total)

    for a in &all {
        let Some(title_id) = a.title_associations.first().and_then(|ta| ta.id.clone()) else {
            continue;
        };
        titles_seen.insert(title_id.clone());
        // Assume any title with achievements tracked server-side is
        // theoretically replayable — we can't check its devices list
        // without the titlehub call, which is optional here.
        titles_replayable.insert(title_id.clone());

        let entry = per_title_counts.entry(title_id).or_default();
        entry.1 += 1; // total
        let achieved = a.progress_state.eq_ignore_ascii_case("Achieved");
        if achieved {
            current += 1;
            entry.0 += 1;
        }
        // Reward gamerscore lives in `rewards[type=Gamerscore].value`.
        if let Some(gs_reward) = a
            .rewards
            .iter()
            .find(|r| r.ty.as_deref() == Some("Gamerscore"))
            .and_then(|r| r.value.as_deref())
            .and_then(|v| v.parse::<i64>().ok())
        {
            total_gs += gs_reward;
            if achieved {
                current_gs += gs_reward;
            }
        }
    }

    let completed_titles = per_title_counts
        .values()
        .filter(|(c, t)| *t > 0 && c >= t)
        .count();

    let mut totals = ProfileTotals {
        total_titles: titles_seen.len(),
        replayable_titles: titles_replayable.len(),
        completed_titles,
        current_achievements: current,
        total_achievements: total,
        current_gamerscore: current_gs,
        total_gamerscore: total_gs,
    };

    // Prefer the authoritative profile-endpoint gamerscore over the
    // bulk-scan sum when it's higher. The scan tally misses rewards
    // whose `value` didn't parse as i64, or whose `type` was "Other"
    // / blank for old titles.
    if let Ok(profile) = profile_res {
        if let Some(person) = profile.people.first() {
            if let Some(gs) = person
                .gamer_score
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
            {
                if gs > totals.current_gamerscore {
                    totals.current_gamerscore = gs;
                    if gs > totals.total_gamerscore {
                        totals.total_gamerscore = gs;
                    }
                }
            }
        }
    }

    Ok(totals)
}

/// `true` if this title can be unlocked through the modern Xbox Live
/// `/achievements/update` endpoint we use. Xbox 360-only, iOS-only,
/// Android-only, and Switch-only titles are NOT replayable.
///
/// Exposed publicly so every surface that displays titles (games list,
/// achievement picker, search results, etc.) can apply the same filter.
pub fn is_title_replayable(devices: &[String]) -> bool {
    if devices.is_empty() {
        // No device metadata at all — trust the achievement count and
        // try it. Better to attempt than to hide something the user
        // could otherwise replay.
        return true;
    }
    devices.iter().any(|d| {
        let d = d.to_ascii_lowercase();
        // Modern consoles / PC. "Win32" is the legacy Windows store
        // category that still lives on the Xbox Live achievement graph,
        // so it's treated the same as "PC".
        d == "pc" || d == "win32" || d == "xboxone" || d == "xboxseries" || d == "cloud"
    })
}

/// Lightweight lookup used by the GUI's "pick games" step. Resolves the
/// gamertag, then returns the target's games list sorted by *most recently
/// played first* (which is how the user expects the picker to order them).
///
/// This is one HTTP call vs. `build_plan`'s potentially thousands, so it's
/// fast enough to run synchronously as the user types.
pub async fn preview_games(client: &XboxClient, target_xuid: &str) -> Result<Vec<GamePreview>> {
    preview_games_with_progress::<fn(usize, Option<i64>)>(client, target_xuid, None).await
}

/// Same as [`preview_games`] but accepts a progress callback that
/// fires as the bulk achievements scan walks pages. Used by the GUI
/// `games.list_deep` / `plan.preview_games` handlers to drive a real
/// progress bar — without this the deep scan looked frozen for
/// minutes on big profiles (a large one measured at 2300 titles,
/// 50k+ achievements, 6+ minutes wall clock).
///
/// `progress` callback signature: `(done_so_far, total_known_or_None)`.
/// `total` is `None` until the FIRST page comes back with
/// `paging_info.total_records`; from then on every callback carries
/// the real total so the UI can compute a percent.
pub async fn preview_games_with_progress<F>(
    client: &XboxClient,
    target_xuid: &str,
    progress: Option<F>,
) -> Result<Vec<GamePreview>>
where
    F: FnMut(usize, Option<i64>) + Send + 'static,
{
    if target_xuid.trim().is_empty() {
        return Err(Error::invalid_argument("empty target_xuid"));
    }
    // Titlehub + bulk-unlocked achievements fetched concurrently so
    // the picker sees EVERY title the target has any unlocks on.
    // Titlehub alone misses titles that Xbox omits from the user's
    // history (gamepass-expired, some ACA NEOGEO / BC titles, etc.)
    // — live-repro'd: on a large profile titlehub doesn't list
    // `1721685011` but the bulk scan + per-title query show it
    // full 12/12. Merging bulk-scan title_ids back in means the
    // picker no longer silently drops real games.
    let (games_res, bulk_res) = tokio::join!(
        client.get_games_list(target_xuid),
        client.get_all_unlocked_achievements_slim_with_progress::<F>(target_xuid, true, progress,),
    );
    let games = games_res?;
    let bulk: Vec<crate::models::SlimAchievement> = bulk_res?;

    // Aggregate bulk-scan data by title_id so we can synthesize
    // rows for titles titlehub omitted.
    let mut bulk_by_title: HashMap<String, (String, i32, i32)> = HashMap::with_capacity(256);
    for a in &bulk {
        let Some(assoc) = a.title_associations.first() else {
            continue;
        };
        let Some(tid) = assoc.id.as_ref() else {
            continue;
        };
        let gs = a
            .rewards
            .iter()
            .find(|r| r.ty.as_deref() == Some("Gamerscore"))
            .and_then(|r| r.value.as_deref())
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(0);
        let entry = bulk_by_title
            .entry(tid.clone())
            .or_insert_with(|| (assoc.name.clone().unwrap_or_default(), 0, 0));
        entry.1 = entry.1.saturating_add(1); // count achievements
        entry.2 = entry.2.saturating_add(gs); // sum gamerscore
    }

    let mut seen: HashSet<String> = HashSet::with_capacity(games.titles.len());
    let mut rows: Vec<GamePreview> = games
        .titles
        .into_iter()
        .filter_map(|t| {
            let tid = t.title_id?;
            let name = t.name.unwrap_or_default();
            let ach = t.achievement.unwrap_or_default();
            // Xbox Live's titlehub endpoint has an annoying quirk: for many
            // (thousands, for large profiles like SiegfriedX) titles it
            // returns `totalAchievements: 0` even though the user has
            // `currentAchievements: N > 0`. Filtering by `total > 0` drops
            // 4701 out of 4797 real-achievement titles on SiegfriedX.
            // The CORRECT filter is "user has ever unlocked something OR
            // the title has achievements defined".
            let has_any = ach.current_achievements > 0
                || ach.total_achievements > 0
                || ach.current_gamerscore > 0
                || ach.total_gamerscore > 0;
            if !has_any {
                return None;
            }
            // Only include titles that are actually replayable via the
            // modern Xbox Live unlock endpoint. Xbox 360-only, iOS-only,
            // Android-only, Nintendo Switch-only etc. are filtered OUT
            // entirely so they never appear in the picker.
            if !is_title_replayable(&t.devices) {
                return None;
            }
            seen.insert(tid.clone());
            Some(GamePreview {
                title_id: tid,
                name,
                current_achievements: ach.current_achievements,
                total_achievements: ach.total_achievements,
                current_gamerscore: ach.current_gamerscore,
                total_gamerscore: ach.total_gamerscore,
                last_time_played: t.title_history.and_then(|h| h.last_time_played),
                devices: t.devices,
            })
        })
        .collect();

    // Merge bulk-scan-only titles: real games with real unlocks
    // that titlehub chose to omit. Totals are unknown (titlehub
    // would be the source), so we MUST NOT inflate `total` to
    // `current` — that would make the UI render a 5/10 game as
    // 5/5 = 100% complete, which is actively misleading.
    //
    // Sentinel: `total_achievements = 0, current_achievements = N`.
    // The UI treats `total == 0 && current > 0` as "total unknown,
    // ≥N unlocked so far" instead of claiming 100% completion.
    // Downstream `build_plan` re-discovers total via its per-title
    // backfill pass (see title_totals backfill in build_plan).
    //
    // devices stays empty — replayability filter is deferred to
    // `build_plan` which applies it on the real bulk scan. Picker
    // showing a non-replayable title is OK; build_plan skips it.
    for (tid, (name, current, current_gs)) in bulk_by_title {
        if seen.contains(&tid) {
            continue;
        }
        rows.push(GamePreview {
            title_id: tid,
            name,
            current_achievements: current,
            total_achievements: 0, // 0 = "unknown" sentinel, NOT "zero total"
            current_gamerscore: current_gs,
            total_gamerscore: 0,
            last_time_played: None,
            devices: Vec::new(),
        });
    }

    // Most recently played first. Entries without a timestamp sink.
    rows.sort_by(|a, b| {
        b.last_time_played
            .as_deref()
            .cmp(&a.last_time_played.as_deref())
    });
    Ok(rows)
}

/// Resolve a gamertag to its Xbox Live XUID using the caller's authenticated
/// client. Works for any public gamertag — the response comes from
/// `profile.xboxlive.com/users/gt(...)/profile/settings`.
pub async fn resolve_gamertag(client: &XboxClient, gamertag: &str) -> Result<String> {
    let v = client.get_gamertag_profile(gamertag).await?;
    v.get("profileUsers")
        .and_then(|u| u.get(0))
        .and_then(|u| u.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            Error::not_found(format!(
                "gamertag `{gamertag}` did not resolve to an Xbox Live profile"
            ))
        })
}

/// Build a chronological plan of every achievement the `target_xuid` profile
/// has unlocked, using the caller's [`XboxClient`] for all HTTP work.
pub async fn build_plan(
    client: &XboxClient,
    target_gamertag: &str,
    target_xuid: &str,
    opts: BuildOptions,
) -> Result<Plan> {
    if target_xuid.trim().is_empty() {
        return Err(Error::invalid_argument("empty target_xuid"));
    }

    // ==== FAST PATH ====
    //
    // Old approach: fetch games list → 4700+ per-title achievement fetches.
    //
    // New approach: fetch games list (for device filter) in parallel with
    // ONE bulk scan of `/users/xuid/achievements?unlockedOnly=true` — which
    // returns every achievement the user has unlocked across every title,
    // paginated at 1000/page. For a SiegfriedX-sized profile (126k
    // achievements) that's 126 requests vs 4700: 37x fewer, 10-100x faster
    // wall-clock, and VERY rate-limit friendly.
    //
    // Devices + title names still come from `get_games_list` so we can
    // apply the X360/mobile filter. The two fetches run concurrently.
    //
    // Note: `include`/`skip` filter sets used to be read here as `&HashSet`
    // borrows, but the parallel filter below lives inside a `spawn_blocking`
    // closure that requires `'static` captures — so we clone them into
    // `include_owned`/`skip_owned` at the filter site instead.

    // Per-page progress reporter. The first page already tells us
    // `totalRecords`, so we emit an accurate percentage from page 2
    // onwards. Fires once per ~1000 items fetched — plenty granular for
    // a human-readable progress bar.
    let progress_cb = opts.progress.clone();
    let started = std::time::Instant::now();
    let on_page = move |items_so_far: usize, total_known: Option<i64>| {
        let total = total_known
            .and_then(|t| usize::try_from(t).ok())
            .unwrap_or(items_so_far.max(1));
        if let Some(f) = progress_cb.as_ref() {
            f(items_so_far, total);
        }
    };

    let (games_res, achs_res) = tokio::join!(
        client.get_games_list(target_xuid),
        // Slim variant: deserializes only the 7 fields we read below.
        // For a 126k-row response that's ~7x less CPU on the parse
        // step AND ~7x less memory pressure downstream.
        //
        // BRUTAL SCAN FIX C1.2/1.3 + C2.2: pass through scan_cache_path
        // so mega profiles can resume stalled scans; pass scan_pool so
        // multi-token throughput kicks in when TokenPool has extras.
        client.get_all_unlocked_achievements_slim_pooled(
            target_xuid,
            true,
            Some(on_page),
            opts.scan_cache_path.clone(),
            opts.scan_pool.clone(),
        ),
    );
    let games = games_res?;
    let all_achievements = achs_res?;

    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        total_titles_in_library = games.titles.len(),
        total_unlocked_achievements = all_achievements.len(),
        "plan bulk scan returned"
    );

    // Index games list for O(1) lookup: title_id → (devices, name).
    // Arc-wrapped so every rayon worker can share the same map without
    // cloning on each lookup (the worker only needs a read-only handle).
    //
    // Also capture per-title completion totals from the SAME
    // titlehub response — the target's achievement progress per
    // title. Feeds `Plan::title_totals` so the "fastest 100%
    // completed games" ranking knows which titles the target
    // actually platinumed vs. touched-and-left.
    let mut title_totals_map: HashMap<String, crate::plan::TitleCompletion> =
        HashMap::with_capacity(games.titles.len());
    let title_meta: Arc<HashMap<String, (Vec<String>, String)>> = Arc::new(
        games
            .titles
            .into_iter()
            .filter_map(|t| {
                let tid = t.title_id?;
                let name = t.name.unwrap_or_default();
                if let Some(ach) = t.achievement.as_ref() {
                    title_totals_map.insert(
                        tid.clone(),
                        crate::plan::TitleCompletion {
                            current_achievements: ach.current_achievements,
                            total_achievements: ach.total_achievements,
                        },
                    );
                }
                Some((tid, (t.devices, name)))
            })
            .collect(),
    );

    // ==== SCID CACHE PREWARM ====
    //
    // The Xbox achievements endpoint ships `serviceConfigId` on every
    // row of the bulk response. Harvest them NOW so that when the user
    // later triggers a replay, `unlock_achievement` can skip the
    // per-title resolve round-trip. For a 200-title profile that's
    // ~16s of hidden latency between "click replay" and "first unlock
    // lands" — this collapses it to ~0ms.
    //
    // De-dup by title_id up front so we don't push 126k duplicate
    // entries into the cache mutex. Capacity sized against the title
    // metadata map (at most one scid per title).
    let mut scid_pairs: HashMap<String, String> = HashMap::with_capacity(title_meta.len());
    for a in &all_achievements {
        let Some(tid) = a.title_associations.first().and_then(|ta| ta.id.as_ref()) else {
            continue;
        };
        if scid_pairs.contains_key(tid) {
            continue;
        }
        // Skip rows that carry only the default scid (serde's fallback
        // when the server omits the field). `warm_scid_cache_bulk`
        // filters these out too, but doing it here saves a string
        // allocation per duplicate.
        if !a.service_config_id.is_empty() {
            scid_pairs.insert(tid.clone(), a.service_config_id.clone());
        }
    }
    client.warm_scid_cache_bulk(scid_pairs.into_iter());

    // ==== PARALLEL FILTER + MAP ====
    //
    // For a SiegfriedX-sized profile (~126k rows) the old serial filter
    // loop took ~400ms on a single core — long enough that the GUI's
    // progress bar visibly pauses at 100% before the plan summary
    // appears. Hoist the loop into a rayon `par_iter` so every logical
    // core shares the work. The counters become AtomicUsize so we can
    // still log the same drop-reason breakdown at the end.
    //
    // `spawn_blocking` wraps the whole thing: rayon's work-stealing
    // pool spins up dedicated OS threads, so we must NOT run it from
    // the tokio runtime thread (that would keep tokio's N-thread pool
    // pinned for the duration of the filter). Since the closure only
    // touches the immutable `all_achievements` + `title_meta` + the
    // filter options, no `&mut` capture is needed.
    let require_ts = opts.require_timestamp;
    let skip_xbox_360 = opts.skip_xbox_360;
    // Include/skip sets live inside Options<HashSet<String>>; move clones
    // into the blocking closure so the closure is 'static.
    let include_owned = opts.include_title_ids.clone();
    let skip_owned = opts.skip_title_ids.clone();
    let title_meta_cl = title_meta.clone();
    let min_unlocked = usize::try_from(opts.min_unlocked).unwrap_or(0);
    let title_unlocked_counts: Option<Arc<HashMap<String, usize>>> = if min_unlocked > 1 {
        let mut counts = HashMap::new();
        for a in &all_achievements {
            let Some(title_id) = a.title_associations.first().and_then(|ta| ta.id.as_ref()) else {
                continue;
            };
            let time_unlocked = a
                .progression
                .as_ref()
                .and_then(|p| p.time_unlocked.as_ref())
                .filter(|t| !t.is_empty() && !t.starts_with("0001-01-01"));
            if a.progress_state.eq_ignore_ascii_case("Achieved") || time_unlocked.is_some() {
                *counts.entry(title_id.clone()).or_insert(0) += 1;
            }
        }
        Some(Arc::new(counts))
    } else {
        None
    };

    let counters = Arc::new(DropCounters::default());
    let counters_cl = counters.clone();
    let safety_policy = opts.safety;
    // Freeze "now" once so every parallel classification uses the
    // same reference point — a scan taking ~60s could otherwise
    // straddle a time-window boundary.
    //
    // BUG HUNTER H6: the prior `unwrap_or(0)` fallback masked a real
    // clock failure. With `now_unix_for_safety = 0` and the strict
    // safety policy active, every achievement whose `time_window.start_date`
    // parses to any timestamp > 0 classifies as `TimeWindowNotYetOpen`
    // → silently filtered → plan comes back empty. User sees "0
    // achievements found" with no indication that the problem is a
    // broken system clock (pre-1970 BIOS, bad VM host, corrupted
    // RTC). Now we refuse to build a plan under a broken clock
    // rather than lie — the error message points at the root cause.
    let now_unix_for_safety: i64 = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => {
            return Err(Error::other(format!(
                "system clock is before the Unix epoch — cannot classify time-windowed achievements \
                 safely (duration_since(UNIX_EPOCH) failed: {e}). Fix the host's date/time and retry."
            )));
        }
    };

    let filter_started = std::time::Instant::now();
    // Size of the raw scan, captured before the parallel filter takes
    // ownership — the denominator for `PlanBuildStats`.
    let scanned_total = all_achievements.len();
    let mut entries: Vec<PlanEntry> = tokio::task::spawn_blocking(move || {
        all_achievements
            .into_par_iter()
            .filter_map(|a| {
                // titleAssociations[0] carries the title id + name.
                let assoc = a.title_associations.first()?;
                let Some(title_id) = assoc.id.clone() else {
                    counters_cl.no_title_assoc.fetch_add(1, Ordering::Relaxed);
                    return None;
                };

                // Replayability filter (X360-only / mobile / switch → drop).
                // Unknown titles (not in games list) are allowed through by
                // default so we don't lose data — Xbox's title metadata is
                // occasionally out of sync with achievement metadata.
                //
                // BORROW, don't clone (audit: filter-clones-devices-vec-per-
                // achievement): this runs once per achievement (~126k on a
                // mega profile). The old `.cloned()` allocated a fresh
                // `Vec<String>` + `String` every iteration even though
                // `devices` is only read here and the game name is cloned
                // lazily below (and usually superseded by `assoc.name`).
                let meta = title_meta_cl.get(&title_id);
                let devices: &[String] = meta.map_or(&[][..], |(d, _)| d.as_slice());
                if skip_xbox_360 && !devices.is_empty() && !is_title_replayable(devices) {
                    counters_cl.non_replayable.fetch_add(1, Ordering::Relaxed);
                    return None;
                }

                if let Some(want) = include_owned.as_ref() {
                    if !want.contains(&title_id) {
                        counters_cl.include_filter.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                }
                if let Some(already) = skip_owned.as_ref() {
                    if already.contains(&title_id) {
                        counters_cl.skip_filter.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                }
                if min_unlocked > 1
                    && title_unlocked_counts
                        .as_ref()
                        .and_then(|counts| counts.get(&title_id).copied())
                        .unwrap_or(0)
                        < min_unlocked
                {
                    counters_cl
                        .min_unlocked_filter
                        .fetch_add(1, Ordering::Relaxed);
                    return None;
                }

                // Only keep entries Xbox marks as actually unlocked AND that
                // carry a real timestamp. The bulk endpoint with
                // `unlockedOnly=true` already enforces the former, but be
                // defensive.
                let time_unlocked = a
                    .progression
                    .as_ref()
                    .and_then(|p| p.time_unlocked.clone())
                    .filter(|t| !t.is_empty() && !t.starts_with("0001-01-01"));
                let is_unlocked =
                    a.progress_state.eq_ignore_ascii_case("Achieved") || time_unlocked.is_some();
                if !is_unlocked {
                    return None;
                }
                if require_ts && time_unlocked.is_none() {
                    counters_cl
                        .missing_timestamp
                        .fetch_add(1, Ordering::Relaxed);
                    return None;
                }

                let game_name = assoc
                    .name
                    .clone()
                    .filter(|s| !s.is_empty())
                    // Clone the games-list name ONLY in the fallback branch
                    // (assoc.name empty/missing) — the common path uses the
                    // per-achievement name and never touches `meta`.
                    .unwrap_or_else(|| meta.map(|(_, name)| name.clone()).unwrap_or_default());
                let gs = a
                    .rewards
                    .iter()
                    .find(|r| r.ty.as_deref() == Some("Gamerscore"))
                    .and_then(|r| r.value.as_deref())
                    .and_then(|v| v.parse::<i32>().ok());

                // ============= UNLOCK-SAFETY GATE #1 ==============
                // Classify BEFORE constructing the PlanEntry. If the
                // achievement is revoked / time-window-expired /
                // not-yet-open, it never lands in the plan. The
                // runner will still re-check at unlock time (gate #2)
                // as belt-and-suspenders. Writes an audit entry
                // per skip so the user can verify what was filtered.
                if safety_policy.is_active() {
                    use crate::plan::safety::{
                        append_skip_audit, classify_achievement, SafetyClass, SkipAuditEntry,
                    };
                    let class = classify_achievement(&a, safety_policy, now_unix_for_safety);
                    if !class.is_safe() {
                        match &class {
                            SafetyClass::Revoked => {
                                counters_cl.safety_revoked.fetch_add(1, Ordering::Relaxed);
                            }
                            SafetyClass::TimeWindowExpired { .. } => {
                                counters_cl
                                    .safety_time_expired
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            SafetyClass::TimeWindowNotYetOpen { .. } => {
                                counters_cl
                                    .safety_time_not_yet_open
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            // We only enter this match after `is_safe`
                            // returned false, so `Safe` is impossible by
                            // the immediate caller's invariant. Any future
                            // change that breaks that invariant (e.g.
                            // `is_safe` returns Safe for a borderline case)
                            // should show up as a loud panic, not a silent
                            // miscount.
                            #[allow(clippy::disallowed_macros)]
                            SafetyClass::Safe => unreachable!("is_safe returned false"),
                            // BBB-Discontinued is a TITLE-level filter
                            // that runs upstream of the per-achievement
                            // gate (see `plan::discontinued`). The
                            // per-achievement `classify_achievement`
                            // never returns this variant — it would
                            // mean a contract violation upstream.
                            #[allow(clippy::disallowed_macros)]
                            SafetyClass::Discontinued => unreachable!(
                                "classify_achievement should never return \
                                 SafetyClass::Discontinued — that's a \
                                 title-level decision made before this gate"
                            ),
                        }
                        append_skip_audit(&SkipAuditEntry {
                            iso_timestamp: iso_now_utc(),
                            reason: class.reason_str().to_string(),
                            title_id: title_id.clone(),
                            title_name: game_name.clone(),
                            achievement_id: a.id.clone(),
                            achievement_name: a.name.clone(),
                            gamerscore: gs.unwrap_or(0),
                            classification: class,
                            // Plan-build time has no runner xuid in
                            // scope — the planner is operating on the
                            // SOURCE target's data, the runner xuid
                            // attaches at replay time. Empty = "plan
                            // build phase" for filter consumers.
                            xuid: String::new(),
                        });
                        return None;
                    }
                }

                Some(PlanEntry {
                    title_id,
                    service_config_id: a.service_config_id,
                    achievement_id: a.id,
                    achievement_name: a.name,
                    game_name,
                    time_unlocked: time_unlocked.unwrap_or_default(),
                    gamerscore: gs,
                })
            })
            .collect::<Vec<PlanEntry>>()
    })
    .await
    .map_err(|e| Error::other(format!("filter task join: {e}")))?;

    tracing::info!(
        elapsed_ms = filter_started.elapsed().as_millis() as u64,
        kept = entries.len(),
        dropped_non_replayable = counters.non_replayable.load(Ordering::Relaxed),
        dropped_include_filter = counters.include_filter.load(Ordering::Relaxed),
        dropped_skip_filter = counters.skip_filter.load(Ordering::Relaxed),
        dropped_min_unlocked_filter = counters.min_unlocked_filter.load(Ordering::Relaxed),
        dropped_missing_timestamp = counters.missing_timestamp.load(Ordering::Relaxed),
        dropped_no_title_assoc = counters.no_title_assoc.load(Ordering::Relaxed),
        safety_revoked = counters.safety_revoked.load(Ordering::Relaxed),
        safety_time_expired = counters.safety_time_expired.load(Ordering::Relaxed),
        safety_time_not_yet_open = counters.safety_time_not_yet_open.load(Ordering::Relaxed),
        "plan entries assembled (parallel)"
    );

    // Same numbers, carried ON the plan instead of only into the log —
    // otherwise "why is this plan smaller than that one?" is
    // unanswerable from the UI, and a heavily-Xbox-360 profile looks
    // like a truncated scan.
    let build_stats = crate::plan::PlanBuildStats {
        scanned: scanned_total,
        non_replayable: counters.non_replayable.load(Ordering::Relaxed),
        missing_timestamp: counters.missing_timestamp.load(Ordering::Relaxed),
        no_title_assoc: counters.no_title_assoc.load(Ordering::Relaxed),
        safety_revoked: counters.safety_revoked.load(Ordering::Relaxed),
        safety_time_window: counters.safety_time_expired.load(Ordering::Relaxed)
            + counters.safety_time_not_yet_open.load(Ordering::Relaxed),
        user_filters: counters.include_filter.load(Ordering::Relaxed)
            + counters.skip_filter.load(Ordering::Relaxed)
            + counters.min_unlocked_filter.load(Ordering::Relaxed),
    };

    // Fire a final 100%/done event so any straggler UI state is cleared.
    if let Some(f) = opts.progress.as_ref() {
        let n = entries.len().max(1);
        f(n, n);
    }

    // 3. Sort chronologically (oldest first → replay preserves order).
    //    `par_sort_unstable_by` uses every core and is significantly
    //    faster than serial `sort_by` on 100k+ inputs. Unstable sort
    //    is fine here because equal-timestamp entries have no defined
    //    relative order anyway.
    //
    // BUG HUNTER L2: sort by PARSED unix-seconds instead of the raw
    // ISO string. Lexicographic string comparison puts
    // `"T00:00:00.500Z"` BEFORE `"T00:00:00Z"` (because `.`=0x2E <
    // `Z`=0x5A), so if Xbox mixed fractional + non-fractional
    // timestamps within the same sub-second (observed historically
    // on legacy 360-BC rows), the chronological order would break.
    // Parsing once per entry up front is cheap (~2ms for 126k
    // entries) and produces a monotonic i64 key that's correct for
    // every format `parse_iso` understands. Entries that fail to
    // parse sort to the front (i64::MIN) — they'd otherwise be
    // silently shuffled by raw-string sort anyway.
    //
    // BUG HUNTER BBB-Timing-1: previously this used
    // `par_sort_unstable_by_key` keyed on JUST the i64 seconds, so
    // bursts of same-second unlocks (very common in legacy 360 titles
    // and inevitable across 240k+-entry aggregate plans) shuffled
    // into a deterministic-but-arbitrary order. The bit-by-bit replay
    // then POSTed those entries out of true chronological order. We
    // now key on `(secs, nanos)` — the parsed sub-second fraction —
    // and use a STABLE sort so equal-key entries preserve their input
    // order as a final tiebreak. Cost: ~5 % more sort time on a 126 k
    // plan; ordering correctness gain is unconditional.
    let sort_started = std::time::Instant::now();
    entries = tokio::task::spawn_blocking(move || {
        // `par_sort_by_cached_key`, not `par_sort_by_key`: the latter
        // re-invokes the key closure O(n log n) times, re-parsing each
        // ISO timestamp (~17 comparisons/element on a 126k plan) via the
        // non-trivial `parse_iso_subsec_public` (trim + splits + integer
        // parses + civil_to_unix, twice). Cached-key evaluates the key
        // exactly ONCE per element and is still a STABLE sort, preserving
        // the (secs, nanos) tiebreak-by-input-order the comment above
        // relies on. Audit: plan-sort-reparses-iso-nlogn.
        entries.par_sort_by_cached_key(|e| {
            crate::plan::runner::parse_iso_subsec_public(&e.time_unlocked).unwrap_or((i64::MIN, 0))
        });
        entries
    })
    .await
    .map_err(|e| Error::other(format!("sort task join: {e}")))?;
    tracing::info!(
        elapsed_ms = sort_started.elapsed().as_millis() as u64,
        n = entries.len(),
        "plan entries sorted"
    );

    // ==== TITLE-TOTALS BACKFILL ====
    //
    // `title_totals_map` was populated from titlehub, but titlehub
    // sometimes omits titles that the bulk achievements scan can
    // see — gamepass-expired, shared-library, demo, and several
    // legacy / arcade entries (empirically: ACA NEOGEO titles,
    // some Xbox 360 BC games) fall into this bucket. Without a
    // total count, the "fastest 100%-completed games" ranking
    // downstream can't classify them as fully completed, so they
    // silently disappear from the summary even when the target
    // actually platinumed them.
    //
    // Fix: after the filter pass, find every title present in
    // `entries` but missing from `title_totals_map` and derive
    // totals directly from the achievements endpoint. Bounded
    // parallelism (8) so the backfill for a 50-title miss costs
    // ~1 round-trip in wall time.
    {
        // Backfill condition: title is in entries AND we lack a
        // USEFUL total for it. "Useful" = `total_achievements > 0`.
        // Titlehub frequently populates title entries with
        // `total=0` (the same bug documented in `preview_games`:
        // "returns totalAchievements: 0 even though
        // currentAchievements > 0"). Earlier versions only backfilled
        // absent keys, which meant titlehub's zero-totals junk
        // blocked the fix. Result observed live on xdzleoz's
        // 8435-entry plan: title_totals had 1328 keys but ZERO
        // passed `is_complete`, and the GUI "Fastest 100%" ranking
        // was permanently empty. Backfilling also re-queries those
        // junk entries and replaces them with real totals.
        let mut missing_titles: Vec<String> = entries
            .iter()
            .map(|e| e.title_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|tid| {
                // Backfill when absent OR when present with stale
                // zero totals. `is_none_or` would be cleaner but
                // requires Rust 1.82; MSRV is 1.75.
                //
                // BUG HUNTER M6: the prior filter also re-queried
                // `current > total` cases ("delisted achievements
                // still credited"). In practice Xbox keeps returning
                // the same junk ratio on subsequent `/achievements`
                // calls — the total is simply wrong in their catalog
                // until a separate backend pass corrects it (days to
                // weeks later, not synchronously). Re-querying these
                // on every plan build wasted ~0.5s/title against the
                // 180s BACKFILL_BUDGET, starving titles with genuinely
                // missing data. Users who want to force-refresh junk
                // ratios can do a full rebuild (resume=false) to clear
                // the plan cache and start over from a clean fetch.
                match title_totals_map.get(tid) {
                    None => true,
                    Some(tc) => tc.total_achievements <= 0,
                }
            })
            .collect();
        missing_titles.sort();
        if !missing_titles.is_empty() {
            let missing_n = missing_titles.len();
            tracing::info!(
                missing_n,
                "title_totals backfill: titles with missing/zero totals — querying per-title"
            );
            // Budget cap. Without it a mega-profile like siegfriedx
            // with ~4000 unique titles × ~0.5s/req ÷ 8 parallel =
            // ~4min of silent work AFTER the scan hit 100% — user
            // sees a frozen GUI and thinks the app crashed.
            // Cap at 3 min; whatever isn't filled in time ships with
            // `total=0`, which the 100%-ranking filter already
            // treats as "unknown" (excludes the title safely).
            #[allow(clippy::items_after_statements)]
            const BACKFILL_BUDGET: Duration = Duration::from_secs(180);
            // Signal the caller's progress hook during backfill too,
            // using synthetic "achievement count" semantics: each
            // completed per-title query counts as +1 toward a
            // `missing_n` total. This repurposes the existing
            // progress callback so the GUI sees "backfilling
            // metadata 234/4000" as the final phase instead of a
            // dead screen between 100% scan and summary render.
            let progress_for_backfill = opts.progress.clone();
            let backfill_started = std::time::Instant::now();
            let sem = Arc::new(tokio::sync::Semaphore::new(8));
            let mut joins: tokio::task::JoinSet<(String, Option<crate::plan::TitleCompletion>)> =
                tokio::task::JoinSet::new();
            for tid in missing_titles {
                let permit = sem.clone();
                let client = client.clone();
                let xuid = target_xuid.to_string();
                joins.spawn(async move {
                    // Propagate semaphore-closed as an early None
                    // result instead of swallowing the Err and
                    // running unbounded HTTP. `.await.ok()` then
                    // drop-on-scope-exit used to mean a closed
                    // semaphore (e.g. shutdown path) let the worker
                    // fire its Xbox call anyway. See audit bug #6.
                    let Ok(_p) = permit.acquire_owned().await else {
                        return (tid, None);
                    };
                    let totals = match client.get_achievements_for_title(&xuid, &tid).await {
                        Ok(resp) => {
                            let total = i32::try_from(resp.achievements.len()).unwrap_or(i32::MAX);
                            let current = i32::try_from(
                                resp.achievements
                                    .iter()
                                    .filter(|a| a.progress_state.eq_ignore_ascii_case("Achieved"))
                                    .count(),
                            )
                            .unwrap_or(i32::MAX);
                            Some(crate::plan::TitleCompletion {
                                current_achievements: current,
                                total_achievements: total,
                            })
                        }
                        Err(e) => {
                            tracing::debug!(
                                title_id = %tid,
                                ?e,
                                "title_totals backfill: per-title query failed"
                            );
                            None
                        }
                    };
                    (tid, totals)
                });
            }
            let mut filled = 0usize;
            let mut processed = 0usize;
            let mut join_errors = 0usize;
            let mut budget_exhausted = false;
            // Drain explicitly: a panicking worker returns
            // `Err(JoinError)`, and the old `while let Some(Ok(...))`
            // pattern would exit the loop at the first such error —
            // leaving the remaining pending workers' results
            // un-consumed and the backfill silently partial. Iterate
            // until `join_next()` returns None (set empty) OR the
            // backfill budget expires — whichever comes first.
            loop {
                // Budget check on every completed item so a mega-
                // profile with 4000 missing titles can't silently
                // burn 10 minutes of the user's time. Per-item
                // check keeps the check cost tiny (1 Instant read).
                if backfill_started.elapsed() > BACKFILL_BUDGET {
                    budget_exhausted = true;
                    tracing::warn!(
                        budget_secs = BACKFILL_BUDGET.as_secs(),
                        filled,
                        processed,
                        missing_n,
                        "title_totals backfill: budget exhausted, \
                         aborting the rest (ranking will treat \
                         unfilled titles as unknown)"
                    );
                    // Abort pending tasks so they don't keep
                    // hammering Xbox in the background.
                    joins.shutdown().await;
                    break;
                }
                // Use a 2s recv timeout so the budget check above
                // fires even when all workers are mid-flight and
                // nothing is landing.
                let next = tokio::time::timeout(Duration::from_secs(2), joins.join_next()).await;
                let Ok(result) = next else {
                    // 2-second tick without a result — loop back
                    // so the outer budget check can fire.
                    continue;
                };
                match result {
                    None => break,
                    Some(Ok((tid, Some(tc)))) => {
                        title_totals_map.insert(tid, tc);
                        filled += 1;
                        processed += 1;
                        // Surface backfill progress via the same
                        // callback the scan used — the GUI can
                        // render "finalizing metadata 234/1328"
                        // instead of appearing frozen.
                        if let Some(cb) = progress_for_backfill.as_ref() {
                            cb(processed, missing_n);
                        }
                    }
                    Some(Ok((_tid, None))) => {
                        processed += 1;
                        if let Some(cb) = progress_for_backfill.as_ref() {
                            cb(processed, missing_n);
                        }
                        // validate succeeded but Xbox gave us nothing
                        // usable — already logged at debug level above.
                    }
                    Some(Err(e)) => {
                        join_errors += 1;
                        // Panic path MUST still advance `processed`
                        // otherwise the UI shows "999/1000 backfill"
                        // forever after a panic and the budget check
                        // can never match processed to missing_n. See
                        // audit bug #23.
                        processed += 1;
                        if let Some(cb) = progress_for_backfill.as_ref() {
                            cb(processed, missing_n);
                        }
                        tracing::warn!(
                            ?e,
                            "title_totals backfill: worker panicked — \
                             continuing with the rest"
                        );
                    }
                }
            }
            if join_errors > 0 {
                tracing::warn!(
                    join_errors,
                    "title_totals backfill: some workers panicked; \
                     backfill is partial"
                );
            }
            tracing::info!(
                requested = missing_n,
                filled,
                processed,
                budget_exhausted,
                elapsed_ms = backfill_started.elapsed().as_millis() as u64,
                "title_totals backfill complete"
            );
            // Fire a final progress event at 100% so the GUI
            // visibly clears the "finalizing metadata" phase even
            // when the last few items reported out of order or
            // the budget tripped early.
            if let Some(cb) = progress_for_backfill.as_ref() {
                let final_done = if budget_exhausted {
                    processed.min(missing_n)
                } else {
                    missing_n
                };
                cb(final_done, missing_n);
            }
        }
    }

    // 4. Optional build-time cap. Truncating AFTER the sort means we keep
    // the OLDEST N — the correct choice for "follow from the beginning".
    if opts.max_entries > 0 && entries.len() > opts.max_entries {
        entries.truncate(opts.max_entries);
    }

    let mut plan = Plan {
        source_gamertag: target_gamertag.to_string(),
        source_xuid: target_xuid.to_string(),
        built_at: iso_now(),
        total: entries.len(),
        entries,
        title_totals: title_totals_map,
        // Populated by the MinutesPlayed fetch below.
        build_stats,
        title_min_completion_secs: std::collections::HashMap::new(),
        // Single-profile plan: the one target xuid is the re-queryable
        // source for live floor lookups at run start.
        source_xuids: vec![target_xuid.to_string()],
    };
    plan.retain_title_totals_for_entries();

    // Real-playtime floor: fetch the target's Xbox `MinutesPlayed` per
    // title (the same "time played" TrueAchievements shows) and store
    // it in SECONDS. This is the authoritative sequential-mode pacing
    // floor — "how long it actually took", not the calendar span
    // between first and last unlock. Best-effort: a failure or a
    // private/absent stat just leaves the map short and the runner
    // falls back to the span. Deduped title list keeps the batch small.
    {
        let unique_titles: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            plan.entries
                .iter()
                .filter_map(|e| {
                    if seen.insert(e.title_id.clone()) {
                        Some(e.title_id.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        match client
            .get_minutes_played_map(target_xuid, &unique_titles)
            .await
        {
            Ok(minutes_by_title) => {
                plan.title_min_completion_secs = minutes_by_title
                    .into_iter()
                    .map(|(tid, mins)| (tid, mins.saturating_mul(60)))
                    .collect();
                tracing::info!(
                    titles = unique_titles.len(),
                    with_playtime = plan.title_min_completion_secs.len(),
                    "captured MinutesPlayed floors for sequential-mode pacing"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "MinutesPlayed fetch failed; sequential mode will fall back to \
                     calendar span for this plan"
                );
            }
        }
    }

    // Final checkpoint runs after every final-shape transform so callers
    // never persist a snapshot that violates the requested cap.
    if let Some(cp) = opts.checkpoint.as_ref() {
        cp(&plan.entries);
    }

    Ok(plan)
}

// `fetch_title_entries` / `spawn_title_fetch` removed — the bulk
// profile-level scan in build_plan makes them obsolete. For callers
// that want a single title's achievements, `XboxClient::get_achievements_for_title`
// is still the right surface (with cache + continuation).

fn iso_now() -> String {
    iso_now_utc()
}

/// Public alias — the safety module + other callers need this format.
/// Kept as a thin wrapper over `iso_now()` so existing call sites
/// don't all have to be renamed at once.
#[must_use]
pub fn iso_now_utc() -> String {
    // Minimal ISO-8601 UTC `YYYY-MM-DDThh:mm:ssZ` without pulling `chrono`.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Compute (year, month, day, hour, minute, second) from a Unix timestamp.
/// Uses the civil-from-days algorithm by Howard Hinnant — correct, fast,
/// no leap-second handling needed for our ISO-8601 stamps.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let h = sod / 3600;
    let mi = (sod % 3600) / 60;
    let s = sod % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, mi, s)
}

// (The old `futures_like::buffer_unordered` helper was inlined into
// `build_plan` so we can emit checkpoints every N completions.)

#[cfg(test)]
mod tests {
    use super::*;

    /// Field-reported: Home showed "35.151 / 2.318 — 1.516,4% complete"
    /// with a progress bar painted clear across the page, and
    /// "0/2.034 games at 100%" beside a gamerscore row reading 100%.
    ///
    /// Cause: titlehub reports a stale (often ZERO) per-title
    /// `total_achievements` for titles the profile never launched —
    /// achievements credited by a replay don't mark a title as played,
    /// but the unlock count still comes back. `compute_profile_totals`
    /// summed both raw, so `current` overtook `total` library-wide.
    ///
    /// This pins the two invariants the UI depends on. The aggregation
    /// itself needs the network, so the arithmetic is reproduced here
    /// exactly as the loop applies it.
    #[test]
    fn profile_totals_never_exceed_100_percent() {
        // (current, total_reported_by_titlehub)
        let titles = [
            (17, 0),  // replayed, never launched -> titlehub says 0
            (12, 50), // normal partial progress
            (30, 30), // genuinely complete
            (8, 5),   // stale total, already surpassed
        ];
        let mut cur: i64 = 0;
        let mut tot: i64 = 0;
        let mut completed = 0usize;
        for (c, t) in titles {
            let t_clamped: i32 = t.max(c); // the fix under test
            cur += i64::from(c);
            tot += i64::from(t_clamped);
            if t_clamped > 0 && c >= t_clamped {
                completed += 1;
            }
        }
        let totals = ProfileTotals {
            total_titles: titles.len(),
            replayable_titles: titles.len(),
            completed_titles: completed,
            current_achievements: cur,
            total_achievements: tot,
            current_gamerscore: 0,
            total_gamerscore: 0,
        };

        assert!(
            totals.current_achievements <= totals.total_achievements,
            "current {} must never exceed total {}",
            totals.current_achievements,
            totals.total_achievements
        );
        assert!(
            totals.completion_ratio() <= 1.0,
            "completion_ratio promises [0.0, 1.0], got {}",
            totals.completion_ratio()
        );
        assert_eq!(
            totals.pending_achievements(),
            50 - 12,
            "only the real gap is pending"
        );
        // The two titles whose reported total was stale/zero, plus the
        // genuinely complete one. Without the clamp the zero-total title
        // could never count and this was 1.
        assert_eq!(totals.completed_titles, 3);

        // Raw (unclamped) data must STILL not break the ratio contract —
        // the clamp inside `completion_ratio` is the last line of defence
        // for any other producer.
        let unclamped = ProfileTotals {
            current_achievements: 35_151,
            total_achievements: 2_318,
            ..Default::default()
        };
        assert!(
            (unclamped.completion_ratio() - 1.0).abs() < f64::EPSILON,
            "ratio must saturate at 1.0, got {}",
            unclamped.completion_ratio()
        );
    }

    #[test]
    fn civil_from_unix_matches_known_date() {
        // 2024-01-01 00:00:00 UTC = 1704067200
        let (y, mo, d, h, mi, s) = civil_from_unix(1_704_067_200);
        assert_eq!((y, mo, d, h, mi, s), (2024, 1, 1, 0, 0, 0));
        // 2026-04-21 09:05:56 UTC
        let (y, mo, d, h, mi, s) = civil_from_unix(1_776_762_356);
        assert_eq!((y, mo, d), (2026, 4, 21));
        assert_eq!((h, mi, s), (9, 5, 56));
    }

    #[test]
    fn iso_now_formats_correctly() {
        let s = iso_now();
        // YYYY-MM-DDThh:mm:ssZ = 20 chars
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[10..11], "T");
    }
}
