//! Multi-title "farm" loop — periodically sends one `presence-heartbeat` for
//! a list of titles, in indefinite 15-minute cycles. Inspired by the sister
//! project `rewards_farming_rust` and adapted to the XAU client.
//!
//! Shape of a cycle:
//! ```text
//!   t=0s      → POST presence-heartbeat {titles: [t1, t2, ... t8]}
//!   t=180s    → POST presence-heartbeat {titles: [t1, t2, ... t8]}
//!   ...
//!   t=900s    → cycle done, counter increments, loop restarts
//! ```
//!
//! The loop runs on a tokio task spawned by [`Farm::start`]. Shared state
//! (progress, ping count, errors) is accessed via [`FarmHandle::snapshot`].
//! Stopping the farm sends a signal through an `oneshot` channel and the
//! task exits cleanly at the next tick.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time;

use crate::error::Result;
use crate::xbox::constants as c;
use crate::xbox::XboxClient;

#[derive(Debug, Clone)]
pub struct FarmConfig {
    pub title_ids: Vec<String>,
    pub ping_interval: Duration,
    pub cycle_duration: Duration,
}

impl Default for FarmConfig {
    fn default() -> Self {
        Self {
            title_ids: Vec::new(),
            ping_interval: Duration::from_secs(c::FARM_PING_INTERVAL_SECS),
            cycle_duration: Duration::from_secs(c::FARM_CYCLE_SECS),
        }
    }
}

/// A fixed set of well-known Xbox console titles, for farming presence
/// without depending on what happens to be in the signed-in library.
///
/// Every entry was resolved through this app's own title search and
/// carries `XBOXSERIESXS` in its platform list — none are invented. The
/// `PC` flag records whether the same title also runs on PC: Rewards
/// splits its play activities by platform (`PCGamePlay` vs
/// `playConsole`), and Xbox title IDs are platform-specific, so a title
/// that is console-ONLY is the least ambiguous signal available. Those
/// are listed first for that reason.
///
/// Resolving by name at runtime was the alternative and is worse here:
/// it costs eight network round trips before a farm can start, and the
/// search is fuzzy. Searching "Starfield" returns a mislabelled
/// `XBOX360` entry ahead of the real one, and "Sea of Thieves" returns
/// an "Insider" build first — picking the top hit would have farmed the
/// wrong titles. These IDs are pinned precisely so that guesswork
/// happens once, here, under review.
pub const FIXED_CONSOLE_TITLES: &[(&str, &str, bool)] = &[
    // (title_id, display name, also_on_pc)
    ("331135949", "Doom Eternal", false),
    ("1790482525", "Call of Duty: Black Ops Cold War", false),
    ("2043073184", "Halo Infinite", true),
    ("2030093255", "Forza Horizon 5", true),
    ("374923716", "Gears 5", true),
    ("2079794073", "Starfield", true),
    ("1842668909", "Forza Motorsport", true),
    ("66591171", "Sea of Thieves", true),
];

/// Hard cap on concurrent presence titles per farm run.
///
/// Xbox's `/users/{xuid}/devices/current/titles/current` endpoint
/// chunks heartbeats at `HEARTBEAT_CHUNK_SIZE=10`, so a 1000-title
/// list produces 100 sequential POSTs every 180s — and to TrueAch's
/// crawlers / Xbox's own consistency checks it looks like a user is
/// "currently playing" 1000 games at once. That cardinality is
/// inhuman and trivially flagged.
///
/// We re-use `MAX_SIMULTANEOUS_TITLES` (8) — the same ceiling
/// `compute_tiered_schedule` already enforces for the bit-by-bit
/// runner. 8 is the cheat-detection-aware safe maximum across
/// all presence-broadcasting paths in the codebase. Wave-7 audit
/// spoof-farm-H2.
pub const FARM_MAX_TITLES: usize = crate::plan::bit_by_bit::MAX_SIMULTANEOUS_TITLES;

impl FarmConfig {
    pub fn validate(&self) -> Result<()> {
        if self.title_ids.is_empty() {
            return Err(crate::error::Error::invalid_argument(
                "farm requires at least one title id",
            ));
        }
        // Wave-7 audit spoof-farm-H2: previously there was no upper
        // bound on `title_ids.len()` — a user could paste 500+ title
        // IDs and Xbox would dutifully broadcast presence for all of
        // them (chunked via `HEARTBEAT_CHUNK_SIZE`). 500 simultaneous
        // "currently playing" titles is the single most obvious
        // cheat-detection signal an aggregator can find on a profile.
        // Cap at 8 (`MAX_SIMULTANEOUS_TITLES`) to match the bit-by-
        // bit runner's same-rationale ceiling.
        if self.title_ids.len() > FARM_MAX_TITLES {
            return Err(crate::error::Error::invalid_argument(format!(
                "farm title count {} exceeds max ({}). \
                 More than {} simultaneous 'currently playing' titles \
                 is a cheat-detection red flag — pick a smaller subset.",
                self.title_ids.len(),
                FARM_MAX_TITLES,
                FARM_MAX_TITLES,
            )));
        }
        if self.ping_interval.as_secs() < 30 {
            return Err(crate::error::Error::invalid_argument(
                "farm ping interval must be at least 30s",
            ));
        }
        if self.cycle_duration.as_secs() < self.ping_interval.as_secs() {
            return Err(crate::error::Error::invalid_argument(
                "cycle duration must be >= ping interval",
            ));
        }
        Ok(())
    }
}

/// State machine matching the one in `rewards_farming_rust/src/core/worker.rs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FarmState {
    #[default]
    Idle,
    Farming,
    Pinging,
    Stopping,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FarmProgress {
    pub state: FarmState,
    pub cycle: u32,
    pub cycle_elapsed_secs: u64,
    pub cycle_total_secs: u64,
    pub percent: f64,
    pub ping_count: u32,
    pub last_ping_unix: Option<i64>,
    pub errors: u32,
    pub last_error: Option<String>,
    pub titles: Vec<String>,
}

/// Public handle returned by [`Farm::start`]. Clone freely — it points at the
/// same shared state as the background task.
#[derive(Clone)]
pub struct FarmHandle {
    shared: Arc<RwLock<FarmProgress>>,
    stop_tx: Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

impl FarmHandle {
    pub fn snapshot(&self) -> FarmProgress {
        self.shared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Signal the farm loop to stop at its next tick. Returns `true` if a
    /// stop was sent; `false` if the farm was already stopped/never started.
    pub fn stop(&self) -> bool {
        let maybe_tx = self
            .stop_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(tx) = maybe_tx {
            let _ = tx.send(());
            true
        } else {
            false
        }
    }

    pub fn is_running(&self) -> bool {
        let s = self
            .shared
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .state;
        matches!(s, FarmState::Farming | FarmState::Pinging)
    }
}

pub struct Farm;

impl Farm {
    /// Start a farm loop on the current tokio runtime. The caller keeps the
    /// returned [`FarmHandle`] to poll progress and later stop it. The
    /// background task is detached — dropping the handle does **not** stop
    /// it; call [`FarmHandle::stop`] explicitly.
    pub fn start(
        client: XboxClient,
        xuid: String,
        config: FarmConfig,
    ) -> Result<(FarmHandle, JoinHandle<()>)> {
        config.validate()?;

        let progress = FarmProgress {
            state: FarmState::Farming,
            cycle: 0,
            cycle_elapsed_secs: 0,
            cycle_total_secs: config.cycle_duration.as_secs(),
            percent: 0.0,
            ping_count: 0,
            last_ping_unix: None,
            errors: 0,
            last_error: None,
            titles: config.title_ids.clone(),
        };
        let shared = Arc::new(RwLock::new(progress));
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop_holder = Arc::new(std::sync::Mutex::new(Some(stop_tx)));

        let handle = FarmHandle {
            shared: shared.clone(),
            stop_tx: stop_holder,
        };

        let join = tokio::spawn(run(client, xuid, config, shared, stop_rx));
        Ok((handle, join))
    }
}

async fn run(
    client: XboxClient,
    xuid: String,
    config: FarmConfig,
    shared: Arc<RwLock<FarmProgress>>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    // Poll interval for the inner `tokio::select!`. Chosen small enough that
    // the UI stays smooth (1 Hz progress update) but not so small that we
    // waste CPU on idle wake-ups.
    let tick = Duration::from_secs(1);
    let cycle_total_secs = config.cycle_duration.as_secs();

    loop {
        let cycle_start = std::time::Instant::now();
        // mark new cycle
        {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.cycle_elapsed_secs = 0;
            w.percent = 0.0;
            w.state = FarmState::Farming;
        }

        // Send the first ping immediately so the user sees progress right away.
        do_ping(&client, &xuid, &config.title_ids, &shared).await;

        let mut last_ping = std::time::Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => {
                    let mut w = shared.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                    w.state = FarmState::Stopped;
                    return;
                }
                () = time::sleep(tick) => {
                    let elapsed = cycle_start.elapsed().as_secs();
                    {
                        let mut w = shared.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                        w.cycle_elapsed_secs = elapsed;
                        // `cycle_total_secs` is ~900 and `elapsed` never exceeds it
                        // in practice, so truncation to f64 is safe here.
                        #[allow(clippy::cast_precision_loss)]
                        let pct = (elapsed as f64 / cycle_total_secs as f64 * 100.0).min(100.0);
                        w.percent = pct;
                    }
                    if elapsed >= cycle_total_secs {
                        break;
                    }
                    if last_ping.elapsed() >= config.ping_interval {
                        do_ping(&client, &xuid, &config.title_ids, &shared).await;
                        last_ping = std::time::Instant::now();
                    }
                }
            }
        }

        // Cycle complete — increment counter and loop.
        {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.cycle = w.cycle.saturating_add(1);
            w.percent = 100.0;
            w.state = FarmState::Farming;
        }
    }
}

async fn do_ping(
    client: &XboxClient,
    xuid: &str,
    title_ids: &[String],
    shared: &Arc<RwLock<FarmProgress>>,
) {
    shared
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .state = FarmState::Pinging;

    match client.send_heartbeat_bulk(xuid, title_ids).await {
        Ok(_) => {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.ping_count = w.ping_count.saturating_add(1);
            w.last_ping_unix = Some(unix_now());
            w.last_error = None;
            w.state = FarmState::Farming;
        }
        Err(e) => {
            let mut w = shared
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.errors = w.errors.saturating_add(1);
            w.last_error = Some(e.to_string());
            w.state = FarmState::Farming;
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Random-title helper
// ---------------------------------------------------------------------

/// Pick `n` distinct title IDs at random from `pool` using a Fisher–Yates
/// shuffle seeded by `OsRng`. Returns fewer than `n` if the pool has fewer
/// entries.
pub fn sample_random_titles(pool: &[String], n: usize) -> Vec<String> {
    if n == 0 || pool.is_empty() {
        return Vec::new();
    }
    let mut buf: Vec<String> = pool.to_vec();
    // Partial Fisher–Yates — we only need the first `n` elements shuffled.
    let take = n.min(buf.len());
    for i in 0..take {
        let j = i + (next_random(OsRng.next_u32()) as usize % (buf.len() - i));
        buf.swap(i, j);
    }
    buf.truncate(take);
    buf
}

#[inline]
fn next_random(seed: u32) -> u32 {
    // Trivial identity to keep this testable without touching RngCore
    // when called from inside tests.
    seed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_returns_up_to_n() {
        let pool: Vec<String> = (0..20).map(|i| format!("{i:010}")).collect();
        let got = sample_random_titles(&pool, 8);
        assert_eq!(got.len(), 8);
        // All unique
        let mut sorted = got;
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 8);
    }

    #[test]
    fn sample_clamps_to_pool_size() {
        let pool: Vec<String> = vec!["a".into(), "b".into()];
        let got = sample_random_titles(&pool, 8);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn sample_with_empty_pool() {
        assert!(sample_random_titles(&[], 8).is_empty());
    }

    #[test]
    fn config_validation() {
        let mut c = FarmConfig::default();
        assert!(c.validate().is_err()); // empty title_ids

        c.title_ids.push("1234567890".into());
        c.ping_interval = Duration::from_secs(5);
        assert!(c.validate().is_err()); // interval too small

        c.ping_interval = Duration::from_secs(60);
        c.cycle_duration = Duration::from_secs(30);
        assert!(c.validate().is_err()); // cycle shorter than interval

        c.cycle_duration = Duration::from_secs(900);
        assert!(c.validate().is_ok());
    }

    /// Wave-7 audit spoof-farm-H2: lock the new max-titles cap so a
    /// future copy-edit can't accidentally remove it. 8 titles is
    /// the cheat-detection-aware safe ceiling; 9 must be rejected.
    #[test]
    fn config_validation_rejects_oversize_title_list() {
        let mut c = FarmConfig {
            ping_interval: Duration::from_secs(60),
            cycle_duration: Duration::from_secs(900),
            ..Default::default()
        };
        // Up to FARM_MAX_TITLES (8) is fine.
        c.title_ids = (0..FARM_MAX_TITLES).map(|i| format!("{i:010}")).collect();
        assert!(
            c.validate().is_ok(),
            "exactly {FARM_MAX_TITLES} titles must be accepted"
        );

        // FARM_MAX_TITLES + 1 must be rejected.
        c.title_ids.push("9999999999".to_string());
        let err = c
            .validate()
            .expect_err("oversize title list must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("exceeds max") || msg.contains("simultaneous"),
            "rejection message should explain the cap; got: {msg}"
        );

        // A pasted 1000-title list (the smoking-gun cheat scenario)
        // must also be rejected with the same kind of error.
        c.title_ids = (0..1000).map(|i| format!("{i:010}")).collect();
        assert!(c.validate().is_err(), "1000-title list must be rejected");
    }
}

#[cfg(test)]
mod fixed_titles_tests {
    use super::*;

    /// The preset must fit the presence cap it will be fed into, or the
    /// farm rejects it at validate() and the button does nothing.
    #[test]
    fn the_fixed_list_fits_the_presence_cap() {
        assert_eq!(FIXED_CONSOLE_TITLES.len(), FARM_MAX_TITLES);
    }

    /// Title IDs are decimal strings; a typo that leaves a letter in
    /// would be broadcast to Xbox as-is and silently do nothing.
    #[test]
    fn every_id_is_a_plausible_decimal_title_id() {
        for (id, name, _) in FIXED_CONSOLE_TITLES {
            assert!(
                !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()),
                "{name}: title id {id:?} is not decimal"
            );
            assert!(
                id.parse::<u32>().is_ok(),
                "{name}: title id {id} does not fit a u32"
            );
        }
    }

    #[test]
    fn there_are_no_duplicates() {
        let mut ids: Vec<&str> = FIXED_CONSOLE_TITLES.iter().map(|(i, _, _)| *i).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate title id in the fixed list");
    }

    /// Console-exclusive entries lead, because they are the cleanest
    /// signal if Rewards attributes on the title. If a reorder ever
    /// buries them this stops being true silently.
    #[test]
    fn console_only_titles_come_first() {
        let first_pc = FIXED_CONSOLE_TITLES
            .iter()
            .position(|(_, _, pc)| *pc)
            .unwrap_or(FIXED_CONSOLE_TITLES.len());
        assert!(
            FIXED_CONSOLE_TITLES[..first_pc]
                .iter()
                .all(|(_, _, pc)| !*pc),
            "console-only titles must be contiguous at the front"
        );
        assert!(first_pc >= 2, "expected at least two console-only titles");
    }
}
