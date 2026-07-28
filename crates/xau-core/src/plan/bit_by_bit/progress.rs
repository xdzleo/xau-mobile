//! The live run snapshot the GUI/CLI poll: `BitByBitProgress`, `BitByBitState`, `BitByBitHandle`, `RecentUnlock`, `SkippedGame`, and their mutators.

use super::*;

/// Max entries kept in the live unlock feed ring. Enough to fill a
/// scrollable panel; the DOM side caps its own render too.
pub(super) const RECENT_UNLOCKS_CAP: usize = 30;

/// One game currently in flight, with everything a live card needs.
///
/// The pre-existing `active_title_*` fields on the snapshot are SINGLE
/// SLOTS — they only describe "the current game" while exactly one runs.
/// With a sequential window > 1 each task overwrote the others, so the
/// UI could not show per-game progress at all. This list is maintained
/// per title by the same drop-guard that owns `active_titles`, so it is
/// correct at any window and self-cleans on every exit path.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActiveTitleLive {
    pub title_id: String,
    pub game: String,
    /// Achievements this run has confirmed for this title so far.
    pub done: usize,
    /// Achievements this run intends to fire for it.
    pub total: usize,
    /// Gamerscore confirmed for this title this run.
    pub gamerscore: i32,
    /// The game's pacing floor in seconds (already speedup-compressed).
    /// `0` outside sequential mode.
    pub floor_secs: u64,
    /// Wall-clock ms when the game went online. With `floor_secs` this
    /// is everything the UI needs to run its own countdown and progress
    /// bar without the backend pushing a tick.
    pub started_ms: i64,
    /// Our real credited playtime (seconds) for this title, refreshed
    /// while the floor gate polls it. `0` until the first poll.
    pub played_secs: u64,
    /// True while this specific game sits at the playtime-floor gate.
    pub gated: bool,
}

/// One line of the live "just unlocked" feed the GUI renders — the
/// per-achievement narrative the user asked for ("visualize the
/// achievements being unlocked"). Newest is pushed to the back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecentUnlock {
    /// Monotonic per-run sequence so the UI can dedupe across ticks.
    pub seq: u64,
    pub ach_id: String,
    /// Numeric Xbox title id. Carried so the GUI can look artwork up on
    /// demand (`achievements.list` keys by title, and the game-art map
    /// from `games.list` keys by title) without the plan having to store
    /// image URLs — which would bloat every saved plan and force a
    /// rebuild of the ones that already exist.
    #[serde(default)]
    pub title_id: String,
    pub game: String,
    pub gamerscore: i32,
    /// Wall-clock ms when it confirmed on the wire.
    pub ts_ms: i64,
}

/// Tick the LIVE progress counters + push a line onto the live unlock
/// feed the instant a single achievement confirms on the wire, so the
/// GUI advances per-achievement in real time AND shows WHICH one just
/// landed. `unlocked_this_run` stays the authoritative total credited
/// per-title by the drain loop; these are the display-only live mirror.
/// Register a game as in-flight, or refresh its card fields. Called
/// once per title at activation; `ActiveLiveGuard` removes it.
pub(super) fn push_active_live(
    shared: &Arc<RwLock<BitByBitProgress>>,
    title_id: &str,
    game: &str,
    total: usize,
    floor_secs: u64,
    started_ms: i64,
) {
    let mut w = shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(e) = w.active_live.iter_mut().find(|e| e.title_id == title_id) {
        e.total = total;
        e.floor_secs = floor_secs;
        e.started_ms = started_ms;
        return;
    }
    w.active_live.push(ActiveTitleLive {
        title_id: title_id.to_string(),
        game: game.to_string(),
        done: 0,
        total,
        gamerscore: 0,
        floor_secs,
        started_ms,
        played_secs: 0,
        gated: false,
    });
}

/// Drop a finished game's card. Called from the same drop-guard that
/// owns `active_titles`, so it runs on success, error, cancel and panic.
pub(super) fn remove_active_live(shared: &Arc<RwLock<BitByBitProgress>>, title_id: &str) {
    let mut w = shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(pos) = w.active_live.iter().position(|e| e.title_id == title_id) {
        w.active_live.remove(pos);
    }
}

/// Mirror the floor gate's state onto this title's card, so a card can
/// say "waiting on playtime" for ITS OWN game rather than sharing one
/// run-wide flag with every sibling.
pub(super) fn set_active_live_gate(
    shared: &Arc<RwLock<BitByBitProgress>>,
    title_id: &str,
    gated: bool,
    played_secs: u64,
) {
    let mut w = shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(e) = w.active_live.iter_mut().find(|e| e.title_id == title_id) {
        e.gated = gated;
        if played_secs > 0 {
            e.played_secs = played_secs;
        }
    }
}

pub(super) fn bump_live_unlock(
    shared: &Arc<RwLock<BitByBitProgress>>,
    ach_id: &str,
    title_id: &str,
    game: &str,
    gamerscore: i32,
) {
    let mut w = shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    w.live_unlocked_this_run = w.live_unlocked_this_run.saturating_add(1);
    w.live_gamerscore_this_run = w.live_gamerscore_this_run.saturating_add(gamerscore);
    w.active_title_done = w.active_title_done.saturating_add(1);
    // Credit THIS title's card. `active_title_done` above is the legacy
    // run-wide counter every task shares — it only means anything at a
    // window of 1, which is why the per-title list exists.
    if let Some(e) = w.active_live.iter_mut().find(|e| e.title_id == title_id) {
        e.done = e.done.saturating_add(1);
        e.gamerscore = e.gamerscore.saturating_add(gamerscore);
    }
    let seq = w.live_unlocked_this_run as u64;
    w.recent_unlocks.push(RecentUnlock {
        seq,
        ach_id: ach_id.to_string(),
        title_id: title_id.to_string(),
        game: game.to_string(),
        gamerscore,
        ts_ms: wallclock_ms(),
    });
    // Keep the ring bounded — drop the oldest.
    let overflow = w.recent_unlocks.len().saturating_sub(RECENT_UNLOCKS_CAP);
    if overflow > 0 {
        w.recent_unlocks.drain(0..overflow);
    }
}

/// Push a skipped/impossible game onto the live registry ring (spec
/// item 1: impossible games REGISTERED AND VISIBLE), de-duplicating by
/// title_id so a title that fails several ways in one run appears once,
/// and mirror it into the run log for the forensic record. Caller holds
/// the write lock.
pub(super) fn push_skipped_game(
    w: &mut BitByBitProgress,
    game: &str,
    title_id: &str,
    reason: &'static str,
) {
    if !title_id.is_empty() && w.skipped_games.iter().any(|s| s.title_id == title_id) {
        return;
    }
    w.skipped_games.push(SkippedGame {
        game: game.to_string(),
        title_id: title_id.to_string(),
        reason: reason.to_string(),
        ts_ms: wallclock_ms(),
    });
    let overflow = w.skipped_games.len().saturating_sub(SKIPPED_GAMES_CAP);
    if overflow > 0 {
        w.skipped_games.drain(0..overflow);
    }
    let ts = wallclock_ms();
    let line = serde_json::json!({
        "kind": "bbb_title_skipped",
        "run_id": w.run_id,
        "title_id": title_id,
        "game": game,
        "reason": reason,
        "ts_ms": ts,
    })
    .to_string();
    let run_id = w.run_id.clone();
    let _ = crate::plan::runner::append_bbb_run_line(&run_id, &line);
    // ALSO append to the cumulative cross-run failures database so every
    // game that couldn't be replicated is registered in one file the
    // user can consult (and, later, pre-skip from). Deduped per (title,
    // run) by the skipped_games ring above, so one line per failure —
    // a persistently-dead title accrues one line per run it fails in.
    let db_line = serde_json::json!({
        "kind": "failed_title",
        "title_id": title_id,
        "game": game,
        "reason": reason,
        "run_id": run_id,
        "ts_ms": ts,
    })
    .to_string();
    let _ = crate::plan::runner::append_failed_title_line(&db_line);
}

/// Publish the wall-clock ms of the next scheduled unlock (or clear it
/// with `None`) so the GUI can show a "next unlock in Xm" countdown
/// during long sequential sleeps instead of looking hung.
pub(super) fn publish_next_unlock_at(
    shared: &Arc<RwLock<BitByBitProgress>>,
    target: Option<(i64, u16)>,
) {
    let at_ms = target
        .map(|(s, ms)| s.saturating_mul(1000).saturating_add(i64::from(ms)))
        .unwrap_or(0);
    let mut w = shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    w.next_unlock_at_ms = at_ms;
}

/// Lifecycle states for a bit-by-bit replay. Serializes as
/// lowercase (`"idle"|"resolving"|"running"|"done"|"stopped"|"error"`)
/// so the GUI's JS comparisons stay in a single label vocabulary.
///
/// The expected transitions:
/// - `Idle` → `Resolving` → `Running` → `Done`
/// - `Idle` → `Resolving` → `Running` → `Stopped` (user pressed Stop)
/// - any → `Error` (panic / fatal HTTP / poisoned state)
///
/// Only `Done`, `Stopped`, and `Error` are TERMINAL — `BitByBitHandle::is_running`
/// returns false for those. `Resolving` and `Running` are LIVE (the runner
/// is doing work and should not be re-spawned).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BitByBitState {
    /// No replay running. The default — also the post-Drop state
    /// after the runner's resources have been cleaned up.
    #[default]
    Idle,
    /// Fetching caller's existing achievements to figure out what's
    /// already unlocked.
    Resolving,
    /// Scheduling + executing the unlock phase.
    Running,
    /// Run completed naturally (hit limit, exhausted eligible, or
    /// drained the plan).
    Done,
    /// User pressed Stop before the run could finish.
    Stopped,
    /// Run hit a fatal error (panic, runtime shutdown, unrecoverable
    /// HTTP failure). `BitByBitProgress.last_error` carries the
    /// human-readable reason.
    Error,
}

/// Snapshot of a bit-by-bit replay. Read via `BitByBitHandle::snapshot`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BitByBitProgress {
    pub state: BitByBitState,
    /// This run's id (`unix_ms-pid`), stamped on every wire event and
    /// used to locate the run's dedicated log file. Empty before start.
    #[serde(default)]
    pub run_id: String,
    /// Plan entries that are eligible to unlock now (scheduled in the
    /// past).
    pub eligible_now: usize,
    /// Plan entries that successfully unlocked this run. Authoritative:
    /// credited per-title in the drain loop from each confirmed outcome.
    pub unlocked_this_run: usize,
    /// Gamerscore earned this run.
    pub gamerscore_this_run: i32,
    /// **Live** unlock count — bumped by +1 the instant each achievement
    /// confirms on the wire, so the progress dock ticks per-achievement
    /// in real time instead of jumping by a whole title's worth when its
    /// task finishes. Reconciled up to `unlocked_this_run` at each title
    /// completion (covers any path that didn't bump live). Display-only;
    /// `unlocked_this_run` remains the authoritative total.
    #[serde(default)]
    pub live_unlocked_this_run: usize,
    /// Live gamerscore counterpart to [`Self::live_unlocked_this_run`].
    #[serde(default)]
    pub live_gamerscore_this_run: i32,
    /// Rolling live feed of the most-recent unlocks (bounded ring, newest
    /// last) so the GUI can show a per-achievement "just unlocked" ticker
    /// instead of only an aggregate counter. See [`RecentUnlock`].
    #[serde(default)]
    pub recent_unlocks: Vec<RecentUnlock>,
    /// Plan entries skipped because they're already unlocked on the
    /// caller's profile.
    pub already_done: usize,
    /// Plan entries whose scheduled unlock time is in the FUTURE —
    /// waiting for the next run.
    pub scheduled_for_future: usize,
    /// Earliest future unlock time across all scheduled-for-future
    /// entries, as a Unix timestamp. 0 if none.
    pub earliest_future_unix: i64,
    /// Titles that couldn't be unlocked because they're event-based
    /// and the Events bundle doesn't cover them.
    pub event_based_skipped_titles: usize,
    /// Titles that hit HTTP 403 `code:21` (security claims rejected) on
    /// every unlock. The field timeline proved this is the AUTH TOKEN
    /// EXPIRING mid-run (clean success→403 split at the token's
    /// lifetime), not a per-title wall — so the runner treats the first
    /// such title as token-death and stops (see `token_expired`).
    #[serde(default)]
    pub unauthorized_skipped_titles: usize,
    /// The run detected the auth token expired mid-run (a title failed
    /// every unlock with 403 code:21). The dispatch/drain loops stop the
    /// run cleanly on this instead of hammering 403 on every remaining
    /// title for hours. Bit-by-bit is stateless-resumable: refresh the
    /// token and press Start to continue from where it stopped.
    #[serde(default)]
    pub token_expired: bool,
    /// Titles that errored out (hard 4xx/5xx from Xbox).
    pub errored_titles: usize,
    pub last_title: Option<String>,
    /// The GAME NAME currently being processed (online/unlocking) right
    /// now — set when a title starts, so the UI can show "▶ NOW: X"
    /// during the hours a game is active in sequential mode. `last_title`
    /// is only set at COMPLETION, so it lags a game the whole time it
    /// runs. Cleared at terminal.
    #[serde(default)]
    pub active_game_name: Option<String>,
    /// Achievements landed so far in the CURRENT game (resets per game).
    #[serde(default)]
    pub active_title_done: usize,
    /// Total achievements this run will fire for the current game.
    #[serde(default)]
    pub active_title_total: usize,
    /// The current game's min plausible completion floor (seconds), so
    /// the UI can show "~2h to 100%". 0 outside sequential mode.
    #[serde(default)]
    pub active_title_floor_secs: u64,
    /// True while the sequential playtime-floor gate is holding the
    /// current game's FINAL achievement, polling OUR MinutesPlayed
    /// until it reaches the floor. Without this flag the GUI has no
    /// idea the runner is deliberately waiting and guesses wrong
    /// ("rate-limit window settling" — the 2.10.3 'travou' report).
    /// Cleared on every gate exit and at terminal.
    ///
    /// Derived from [`Self::floor_gate_waiting`], never assigned
    /// directly: with a sequential window > 1 several games can be
    /// parked at their own gate simultaneously, and a plain
    /// `= false` on the first exit would clear the badge while the
    /// others are still waiting — re-introducing the exact 'travou'
    /// confusion this flag was added to kill.
    #[serde(default)]
    pub floor_gate_active: bool,
    /// How many games are parked at the playtime-floor gate right now
    /// (0, or up to the sequential window). The counter is what makes
    /// the flag above safe under concurrency.
    #[serde(default)]
    pub floor_gate_waiting: u32,
    /// Seconds the floor gate has been waiting so far (lag absorption
    /// on top of the pacing that already consumed ~floor seconds).
    #[serde(default)]
    pub floor_gate_waited_secs: u64,
    /// Our real credited playtime (seconds) as of the gate's last
    /// MinutesPlayed poll — what Xbox actually shows as TIME PLAYED.
    #[serde(default)]
    pub floor_gate_played_secs: u64,
    pub last_error: Option<String>,
    /// Phase-1 (`Resolving`) progress: achievements fetched so far.
    /// Updates live as the bulk-scan pages land. Zeroed when the
    /// run enters `Running`. Lets the GUI show "Loading your
    /// library: 4217/12800" instead of a frozen spinner on large
    /// profiles. Audit bug BBB-#4.
    #[serde(default)]
    pub resolve_fetched: usize,
    /// Phase-1 total (total achievements on the caller's profile).
    /// `None` until page 1 returns `paging_info.total_records`,
    /// then populated. Lets the GUI switch from indeterminate to
    /// determinate progress mid-scan.
    #[serde(default)]
    pub resolve_total: Option<i64>,
    /// Whether the presence keepalive loop is active in this run.
    /// Mirrors `BitByBitConfig.presence_keepalive` on start, then
    /// flips to `false` when the run terminates and the DELETE
    /// has been issued. Lets the GUI surface a "🟢 Xbox presence
    /// active" badge while the run is broadcasting and "⚪ Idle"
    /// otherwise, without an extra IPC round-trip.
    #[serde(default)]
    pub presence_keepalive_active: bool,
    /// Maximum speedup tier in effect for this run (read at start,
    /// constant for the run's lifetime). Surfaced so the GUI can
    /// render `tier=2x` next to the rate cell — confirms what
    /// the user actually started after the typed dropdown +
    /// payload + IPC + parse path.
    #[serde(default)]
    pub max_speedup: MaxSpeedup,
    /// Effective sequential window for this run — how many games run
    /// at once (read at start, constant, already clamped). `1` is the
    /// classic one-game-at-a-time mode; `0` outside sequential mode.
    ///
    /// The GUI needs this to render truthfully: the per-game fields
    /// above (`active_game_name`, `active_title_done/total`) are
    /// single-slot and only describe "the current game" when the
    /// window is 1. Above that, the UI should render the live
    /// `active_titles` list instead of pretending there is one.
    #[serde(default)]
    pub sequential_window: usize,
    /// Per-game live state for every title currently in flight — the
    /// list the GUI renders its progress cards from. Correct at ANY
    /// sequential window, unlike the single-slot `active_title_*`
    /// fields, which only describe one game.
    #[serde(default)]
    pub active_live: Vec<ActiveTitleLive>,
    /// Names of titles currently being processed by the dispatch
    /// loop. Bounded by `title_concurrency` (max 8 — one entry
    /// per active per-title worker). Updated by `process_title`'s
    /// entry/exit. Lets the GUI render "currently processing: A,
    /// B, C" instead of just `last_title` (which only shows the
    /// MOST RECENTLY completed title — confusing during a parallel
    /// run because the user sees one title while 5 others are
    /// also in flight).
    #[serde(default)]
    pub active_titles: Vec<String>,
    /// Games skipped this run because they can't be replicated (event-
    /// based / code:49, token-expiry, hard errors, discontinued),
    /// newest last, bounded ring. The spec requires impossible games to
    /// be REGISTERED AND VISIBLE — this drives the named live list so
    /// the user sees WHICH games and WHY, not just a "⏭N" counter.
    #[serde(default)]
    pub skipped_games: Vec<SkippedGame>,
    /// Wall-clock ms at which the next unlock is scheduled to fire
    /// (the floor-adjusted target), or 0 when not pacing. Lets the GUI
    /// show "next unlock in 12m" during the long sequential sleeps
    /// instead of looking hung. Cleared after each POST + at title end.
    #[serde(default)]
    pub next_unlock_at_ms: i64,
    /// Count of titles that reached 100% WITHOUT the playtime floor
    /// being confirmed on the real stat (gate gave up best-effort).
    /// Spec item 4 ("time played must correspond to the speedrunner's")
    /// is not provable for these — surfaced so it's never silent.
    #[serde(default)]
    pub floor_gate_giveups: usize,
    /// Most recent floor-gate give-up (game + reason), for the trouble
    /// tooltip.
    #[serde(default)]
    pub last_floor_gate_giveup: Option<String>,
    /// Titles pre-skipped this run because the failures database already
    /// recorded them as deterministic code:49 (known-impossible). They
    /// never even get the ~1s probe. 0 when nothing was pre-skipped.
    #[serde(default)]
    pub pre_skipped_known_dead: usize,
}

/// One impossible/skipped game, for the live registry the spec demands.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SkippedGame {
    pub game: String,
    pub title_id: String,
    /// Stable slug: `event_based` / `unauthorized` / `error` /
    /// `discontinued`.
    pub reason: String,
    pub ts_ms: i64,
}

/// Cap on the skipped-games ring (same discipline as recent_unlocks).
pub(super) const SKIPPED_GAMES_CAP: usize = 60;

/// Handle to a running bit-by-bit task. Clonable — share between the
/// progress renderer and the stop button.
///
/// Stop signalling uses `tokio::sync::watch::Sender<bool>` rather than
/// a one-shot so that every spawned task (presence loop, per-title
/// workers, per-entry sleep coroutines) can clone a receiver and
/// observe cancellation *immediately*, not just when their next
/// `join_next` boundary lands. The old oneshot-based stop left long
/// `sleep_until_scheduled` calls running for minutes/hours after the
/// user clicked Stop — see audit bug BBB-Sleep-Cancel#1.
/// Knobs the caller may turn WHILE a run is in flight.
///
/// Everything else in [`BitByBitConfig`] is captured once at start and
/// stays fixed for the run — changing e.g. the gap mode halfway would
/// leave already-scheduled titles on one set of rules and new ones on
/// another. These two are safe to move because they are re-read at
/// natural boundaries and never rewrite work already committed:
///
/// * **window** — read each dispatch iteration when deciding whether to
///   reap. Lowering it doesn't kill running games; it stops NEW ones
///   starting until the count falls.
/// * **speedup** — read when a title's floor is computed, i.e. as it
///   starts. Games already pacing keep the floor they began with, since
///   their per-achievement schedule was derived from it.
#[derive(Debug, Clone)]
pub struct LiveControls {
    window: Arc<std::sync::atomic::AtomicUsize>,
    /// `MaxSpeedup` encoded as its ladder index (see `MaxSpeedup::all`).
    speedup: Arc<std::sync::atomic::AtomicU8>,
}

impl LiveControls {
    pub(super) fn new(window: usize, speedup: MaxSpeedup) -> Self {
        Self {
            window: Arc::new(std::sync::atomic::AtomicUsize::new(window)),
            speedup: Arc::new(std::sync::atomic::AtomicU8::new(speedup_to_index(speedup))),
        }
    }
    pub(super) fn window(&self) -> usize {
        self.window.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub(super) fn speedup(&self) -> MaxSpeedup {
        speedup_from_index(self.speedup.load(std::sync::atomic::Ordering::Relaxed))
    }
    /// Clamped to `1..=MAX_SIMULTANEOUS_TITLES`; returns what was set.
    pub fn set_window(&self, n: usize) -> usize {
        let n = n.clamp(1, super::config::MAX_SIMULTANEOUS_TITLES);
        self.window.store(n, std::sync::atomic::Ordering::Relaxed);
        n
    }
    pub fn set_speedup(&self, tier: MaxSpeedup) {
        self.speedup
            .store(speedup_to_index(tier), std::sync::atomic::Ordering::Relaxed);
    }
}

fn speedup_to_index(t: MaxSpeedup) -> u8 {
    u8::try_from(
        MaxSpeedup::all()
            .into_iter()
            .position(|x| x == t)
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

fn speedup_from_index(i: u8) -> MaxSpeedup {
    MaxSpeedup::all()
        .get(i as usize)
        .copied()
        .unwrap_or(MaxSpeedup::Off)
}

#[derive(Clone)]
pub struct BitByBitHandle {
    // pub(super): constructed by `dispatch::start_bit_by_bit`; still
    // private to the rest of the workspace (external API unchanged).
    pub(super) shared: Arc<RwLock<BitByBitProgress>>,
    pub(super) stop_tx: tokio::sync::watch::Sender<bool>,
    /// Live knobs — see [`LiveControls`]. Cloned into the run task, so
    /// a write here is observed by the dispatcher on its next boundary.
    pub(super) live: LiveControls,
}

impl BitByBitHandle {
    /// Live knobs for a run in flight (simultaneous games, speedup
    /// tier). Safe to call at any time; a stopped run simply ignores
    /// them.
    #[must_use]
    pub fn live(&self) -> &LiveControls {
        &self.live
    }

    /// Current progress. The two live knobs are overlaid from
    /// [`LiveControls`] rather than read from the stored snapshot: the
    /// stored copy is written once at the Running transition, so after
    /// a mid-run change the UI would otherwise keep echoing the value
    /// the run STARTED with while the dispatcher acted on the new one.
    /// Overlaying here keeps them true without adding a write on every
    /// dispatch iteration.
    pub fn snapshot(&self) -> BitByBitProgress {
        let mut snap = self.snapshot_raw();
        snap.max_speedup = self.live.speedup();
        if snap.sequential_window > 0 {
            snap.sequential_window = self.live.window();
        }
        snap
    }

    fn snapshot_raw(&self) -> BitByBitProgress {
        // BBB-Poison: recover from a poisoned RwLock instead of
        // re-panicking. The IPC poller in `xau-gui` calls this
        // every second; if a prior write-side panic poisoned the
        // lock, an `expect()` here would kill the poller silently
        // and freeze the GUI on the last good snapshot — exactly
        // the zombie state the BBB-Poison fix was meant to prevent.
        self.shared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Signal the runner to stop on the next iteration of the
    /// per-title dispatch / drain loops. Returns `true` if this call
    /// flipped the stop flag for the first time, `false` if Stop
    /// was already pending. Idempotent — callers can spam the
    /// button safely. The actual termination (`state` flipping to
    /// `Stopped`) is async — poll `is_running()` to know when the
    /// runner has fully drained.
    ///
    /// Note: this only flips the internal `watch::Sender<bool>` flag
    /// the runner observes; the `state` field on `BitByBitProgress`
    /// stays `Running` until the runner actually drains and calls
    /// `reconcile_terminal_state(Stopped)`.
    pub fn stop(&self) -> bool {
        if *self.stop_tx.borrow() {
            return false;
        }
        self.stop_tx.send(true).is_ok()
    }

    /// True while the runner is actively doing work (`Resolving` or
    /// `Running`). Flips to false when the run reaches any terminal
    /// state (`Done`, `Stopped`, `Error`) — that's the GUI's
    /// signal to stop polling for progress and emit the final
    /// summary. Survives a poisoned `RwLock` (recovers via
    /// `PoisonError::into_inner`) so the GUI's progress emitter
    /// keeps working even after a worker panic.
    pub fn is_running(&self) -> bool {
        matches!(
            self.shared
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .state,
            BitByBitState::Resolving | BitByBitState::Running
        )
    }
}
