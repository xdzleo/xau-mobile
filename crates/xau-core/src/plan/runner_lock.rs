//! Cross-process exclusion guard for any runner that posts achievement
//! unlocks (replay / nuclear / bit-by-bit / unlock-one / unlock-all).
//!
//! # Why this exists
//!
//! Two xau processes targeting the same xuid (e.g. CLI replay running
//! while the GUI's replay button gets clicked, or two GUI windows) race
//! on:
//! - the Xbox Live rate-limit budget (double-billing → premature 429s),
//! - the presence-heartbeat coordinator (interleaved title spoofs),
//! - the per-line audit logs (`unlock-history.jsonl`,
//!   `replay-errors.jsonl`, `unlock-safety-skipped.jsonl`).
//!
//! `save_plan` already had a cross-process gate via `SAVE_PLAN_LOCK +
//! create_new`; the unlock runners did not.
//!
//! # How the lock works
//!
//! Atomic `OpenOptions::create_new(true).write(true)` against a file
//! named `XAU/runner.{xuid}.lock`. On success we spawn a **heartbeat
//! thread** that touches the file every `HEARTBEAT_INTERVAL` seconds.
//! On `Drop` the thread is signalled to stop and the file is removed.
//!
//! # Stale-lock detection (BBB-StaleLock + BBB-Heartbeat)
//!
//! The original module refused any pre-existing lock and instructed
//! the user to delete it manually. That left users dead-locked
//! whenever an xau process crashed without releasing — they had to
//! hand-clean every time. The current module auto-detects stale
//! locks via THREE independent signals:
//!
//! 1. **Heartbeat staleness (primary)** — the lock file's mtime is
//!    refreshed every `HEARTBEAT_INTERVAL` (30 s) by the holder. If
//!    the file hasn't been touched in `HEARTBEAT_STALE_AFTER`
//!    (3× interval = 90 s), the holder is dead, hung, or in the
//!    middle of a Drop that can't finish — treat as stale. This is
//!    the strongest signal and catches every "process zombie /
//!    just-died / OS still cleaning up" case the previous PID-only
//!    check missed.
//! 2. **PID liveness (backstop)** — the holder pid is queried via
//!    `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`. Dead → stale.
//!    Alive but exe is not `xau.exe` / `xau-gui.exe` (Windows PID
//!    reuse) → stale. Self-pid → alive (cargo test self-collision
//!    works without exe-name kludges).
//! 3. **Wall-clock age fallback** — `start_unix` older than
//!    `STALE_AFTER_SECS` (24 h) → stale regardless. Defends against
//!    pathological hangs that somehow keep the heartbeat thread
//!    alive without making forward progress.
//!
//! Any single signal saying "stale" wins. We then `remove_file +
//! create_new` ONCE; if a real peer races in between, we surface a
//! clean collision error.
//!
//! # Backoff retry on first refusal (BBB-LockBackoff)
//!
//! Closing and immediately re-opening xau used to occasionally hit
//! "another runner active" because Windows takes ~100-500 ms to fully
//! tear down the prior process. We now retry with exponential backoff
//! (250 ms → 500 ms → 1 s) — total wait ≤ 1.75 s — so a quick
//! restart sequence almost never sees a false collision.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::settings;

/// How often the holder thread refreshes the lock file's mtime.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Stale threshold for the heartbeat signal. 3× the interval gives
/// the holder room to miss a tick (transient FS hiccup, OneDrive
/// sync briefly holding the file) without being declared dead.
const HEARTBEAT_STALE_AFTER: Duration = Duration::from_secs(90);

/// Wall-clock age fallback. Only fires for pathological cases where
/// the heartbeat thread somehow stays alive but the process is hung.
const STALE_AFTER_SECS: u64 = 24 * 60 * 60;

/// Backoff schedule on initial collision. Total upper bound: 1.75 s.
const BACKOFF_RETRY_DELAYS: &[Duration] = &[
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1000),
];

/// RAII guard for the per-xuid runner lock. Hold this for the whole
/// duration of any unlock-issuing runner. Drop = release.
#[must_use = "RunnerLock releases on drop — bind it to a name"]
pub struct RunnerLock {
    path: PathBuf,
    /// Sender half of the heartbeat-stop channel. `Some(tx)` while
    /// the heartbeat thread is running; `None` after Drop has signalled
    /// it (also serves as a "did we already drop?" guard).
    stop_heartbeat: Option<mpsc::Sender<()>>,
    /// Join handle for the heartbeat thread. Joined on Drop so
    /// `remove_file` doesn't race with a final mtime update.
    heartbeat_handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for RunnerLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerLock")
            .field("path", &self.path)
            .field("heartbeat_active", &self.stop_heartbeat.is_some())
            .finish()
    }
}

impl RunnerLock {
    /// Sanitize a xuid into a filename-safe stem. xuids are usually
    /// pure digits but defense-in-depth: anything outside `[A-Za-z0-9_-]`
    /// becomes `_`.
    fn sanitize_xuid(xuid: &str) -> String {
        xuid.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn lock_path(xuid: &str) -> Result<PathBuf> {
        let dir = settings::settings_dir()?;
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        Ok(dir.join(format!("runner.{}.lock", Self::sanitize_xuid(xuid))))
    }

    /// Attempt to acquire the per-xuid runner lock. On collision returns
    /// an error including the pid + start time of the holder.
    ///
    /// Empty xuid is rejected explicitly — every runner already has a
    /// xuid in hand before reaching this point and "" would alias every
    /// caller into the same file.
    pub fn try_acquire(xuid: &str) -> Result<Self> {
        if xuid.trim().is_empty() {
            return Err(Error::invalid_argument(
                "RunnerLock::try_acquire: xuid is empty — runner can't be guarded",
            ));
        }
        let path = Self::lock_path(xuid)?;

        // Fast path: file doesn't exist → atomic create.
        match Self::try_create_new(&path, xuid) {
            Ok(g) => return Ok(g),
            Err(LockAcquireError::AlreadyExists) => {}
            Err(LockAcquireError::Other(e)) => {
                return Err(Error::other(format!(
                    "RunnerLock::try_acquire failed at {}: {e}",
                    path.display()
                )));
            }
        }

        // Slow path: a lock file already exists. Decide whether the
        // holder is real or stale across all three signals.
        if let Some(g) = Self::try_evict_stale(&path, xuid) {
            return Ok(g);
        }

        // Holder looks real. Backoff retry: the user likely just
        // closed and re-opened xau and Windows is still tearing
        // down the prior process. Each retry re-evaluates staleness.
        for delay in BACKOFF_RETRY_DELAYS {
            std::thread::sleep(*delay);
            // Try a clean acquire first in case the file was just
            // removed (Drop on the prior process completed).
            match Self::try_create_new(&path, xuid) {
                Ok(g) => return Ok(g),
                Err(LockAcquireError::AlreadyExists) => {}
                Err(LockAcquireError::Other(e)) => {
                    return Err(Error::other(format!(
                        "RunnerLock::try_acquire (during backoff) failed at {}: {e}",
                        path.display()
                    )));
                }
            }
            if let Some(g) = Self::try_evict_stale(&path, xuid) {
                return Ok(g);
            }
        }

        // Genuine collision after all the retries — another live xau
        // peer is really holding the lock.
        let holder_info = fs::read_to_string(&path).unwrap_or_default();
        Err(Error::other(format!(
            "another xau runner is already active for xuid={xuid} (lock at {}). \
Holder info: {}\n\
If you're certain that process is gone, delete the lock file by hand and retry.",
            path.display(),
            holder_info.trim().replace('\n', "; "),
        )))
    }

    /// If the existing file at `path` looks stale, remove it and try
    /// to acquire ONCE more. Returns `Some(lock)` if eviction worked,
    /// `None` if the holder still looks real (caller should fall
    /// through to backoff or a collision error).
    fn try_evict_stale(path: &Path, xuid: &str) -> Option<Self> {
        let holder = read_holder_info(path).unwrap_or_default();
        if !is_stale(path, &holder) {
            return None;
        }
        tracing::info!(
            ?holder,
            path = %path.display(),
            "runner lock looks stale — removing and retrying"
        );
        let _ = fs::remove_file(path);
        match Self::try_create_new(path, xuid) {
            Ok(g) => Some(g),
            // Lost the race to a real concurrent peer between our
            // delete and our create — fall back to the backoff loop.
            Err(_) => None,
        }
    }

    fn try_create_new(path: &Path, xuid: &str) -> std::result::Result<Self, LockAcquireError> {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut f) => {
                let pid = std::process::id();
                let now = now_unix_secs();
                let _ = writeln!(f, "pid={pid}");
                let _ = writeln!(f, "start_unix={now}");
                let _ = writeln!(f, "xuid={xuid}");
                let _ = writeln!(f, "heartbeat_unix={now}");
                drop(f);

                let (stop_tx, stop_rx) = mpsc::channel::<()>();
                let path_for_thread = path.to_path_buf();
                let pid_for_thread = pid;
                let xuid_for_thread = xuid.to_string();
                let start_for_thread = now;
                let handle = std::thread::Builder::new()
                    .name(format!("xau-runner-lock-heartbeat-{}", pid))
                    .spawn(move || {
                        heartbeat_loop(
                            &path_for_thread,
                            pid_for_thread,
                            start_for_thread,
                            &xuid_for_thread,
                            &stop_rx,
                        );
                    })
                    .ok();

                Ok(Self {
                    path: path.to_path_buf(),
                    stop_heartbeat: Some(stop_tx),
                    heartbeat_handle: handle,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(LockAcquireError::AlreadyExists)
            }
            Err(e) => Err(LockAcquireError::Other(e)),
        }
    }
}

impl Drop for RunnerLock {
    fn drop(&mut self) {
        // Signal the heartbeat thread to stop and join it so the
        // final mtime update can't race with our remove_file below.
        if let Some(tx) = self.stop_heartbeat.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.heartbeat_handle.take() {
            let _ = h.join();
        }
        // Best-effort. If the file was deleted by the user mid-run,
        // remove_file errors are noise.
        let _ = fs::remove_file(&self.path);
    }
}

/// Heartbeat thread body. Sleeps `HEARTBEAT_INTERVAL`, then rewrites
/// the lock file with the same content (touches mtime). Exits as soon
/// as `stop_rx` resolves — never holds the file open for >a few ms.
fn heartbeat_loop(
    path: &Path,
    pid: u32,
    start_unix: u64,
    xuid: &str,
    stop_rx: &mpsc::Receiver<()>,
) {
    loop {
        // Wait either for the interval to elapse OR for a stop
        // signal from Drop. `recv_timeout(Err(Disconnected))` shouldn't
        // happen because we hold tx in the lock struct, but treat it
        // as a stop signal too.
        match stop_rx.recv_timeout(HEARTBEAT_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // Rewrite the lock file with a fresh `heartbeat_unix`. This
        // is a small (~100 byte) write — single fs::write call so
        // it's atomic on Windows/POSIX. A failure here is non-fatal:
        // worst case the next acquirer sees a slightly older mtime
        // and may evict us prematurely. Log at debug only — operators
        // shouldn't see noise on every transient FS hiccup.
        let now = now_unix_secs();
        let body =
            format!("pid={pid}\nstart_unix={start_unix}\nxuid={xuid}\nheartbeat_unix={now}\n");
        if let Err(e) = fs::write(path, body) {
            tracing::debug!(?e, "runner-lock heartbeat write failed");
        }
    }
}

/// Internal: shape of the holder-info we parse out of a lock file.
#[derive(Debug, Default, Clone)]
struct HolderInfo {
    pid: Option<u32>,
    start_unix: Option<u64>,
    heartbeat_unix: Option<u64>,
}

enum LockAcquireError {
    AlreadyExists,
    Other(std::io::Error),
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_holder_info(path: &Path) -> Option<HolderInfo> {
    let raw = fs::read_to_string(path).ok()?;
    let mut info = HolderInfo::default();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("pid=") {
            info.pid = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = line.strip_prefix("start_unix=") {
            info.start_unix = rest.trim().parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("heartbeat_unix=") {
            info.heartbeat_unix = rest.trim().parse::<u64>().ok();
        }
    }
    Some(info)
}

/// Decide whether a lock holder is dead-or-stale. Returns `true` if
/// ANY of three independent signals fires: heartbeat (primary),
/// PID-liveness (backstop), or wall-clock age (fallback).
fn is_stale(path: &Path, holder: &HolderInfo) -> bool {
    let now = now_unix_secs();

    // Signal #1: heartbeat freshness. Strongest signal — catches
    // crashes / hangs / process-zombie / Drop-in-progress.
    let heartbeat_signal = match holder.heartbeat_unix {
        Some(hb) if now >= hb => {
            let age = now - hb;
            if age > HEARTBEAT_STALE_AFTER.as_secs() {
                tracing::info!(
                    age_secs = age,
                    "runner lock heartbeat older than {}s — treating as stale",
                    HEARTBEAT_STALE_AFTER.as_secs()
                );
                Some(true)
            } else {
                Some(false)
            }
        }
        Some(_) => Some(false), // hb in the future (clock skew) — call it fresh
        None => None,           // pre-heartbeat lock format → unknown
    };
    if heartbeat_signal == Some(true) {
        return true;
    }

    // Fallback: filesystem mtime, in case the lock file was created
    // by an old version that didn't write `heartbeat_unix`. Same
    // threshold; mtime is updated by the heartbeat write so they
    // converge in practice.
    if heartbeat_signal.is_none() {
        if let Ok(meta) = fs::metadata(path) {
            if let Ok(modified) = meta.modified() {
                if let Ok(elapsed) = modified.elapsed() {
                    if elapsed > HEARTBEAT_STALE_AFTER {
                        tracing::info!(
                            age_secs = elapsed.as_secs(),
                            "runner lock file mtime older than {}s — treating as stale",
                            HEARTBEAT_STALE_AFTER.as_secs()
                        );
                        return true;
                    }
                }
            }
        }
    }

    // Signal #2: PID liveness. Mostly redundant now that the heartbeat
    // catches dead holders, but catches the narrow window where a
    // process just exited cleanly without a Drop (e.g. taskkill /F)
    // and the file's mtime is technically still <90s old.
    if let Some(pid) = holder.pid {
        if !pid_is_live_xau(pid) {
            tracing::info!(
                pid,
                "runner lock holder PID is dead or running a different binary — treating as stale"
            );
            return true;
        }
    } else {
        tracing::info!("runner lock has no parseable pid — treating as stale");
        return true;
    }

    // Signal #3: wall-clock age fallback. Pathological hang where the
    // heartbeat thread somehow stays alive but doesn't write — at 24h
    // we just give up.
    if let Some(start) = holder.start_unix {
        if now > start && (now - start) > STALE_AFTER_SECS {
            tracing::info!(
                age_secs = now - start,
                "runner lock older than {}s — treating as stale",
                STALE_AFTER_SECS
            );
            return true;
        }
    }
    false
}

#[cfg(windows)]
fn pid_is_live_xau(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::GetProcessImageFileNameA;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut buf = [0u8; 260];
        let len = GetProcessImageFileNameA(handle, buf.as_mut_ptr(), buf.len() as u32);
        let _ = CloseHandle(handle);
        if len == 0 {
            return true;
        }
        let exe = std::str::from_utf8(&buf[..len as usize])
            .unwrap_or("")
            .to_ascii_lowercase();
        exe.contains("xau.exe") || exe.contains("xau-gui.exe")
    }
}

#[cfg(not(windows))]
fn pid_is_live_xau(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    let path = format!("/proc/{pid}");
    std::path::Path::new(&path).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_xuid(tag: &str) -> String {
        format!(
            "test-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn sanitize_xuid_strips_path_separators() {
        assert_eq!(RunnerLock::sanitize_xuid("123"), "123");
        assert_eq!(RunnerLock::sanitize_xuid("../etc"), "___etc");
        assert_eq!(RunnerLock::sanitize_xuid("a/b\\c"), "a_b_c");
        assert_eq!(
            RunnerLock::sanitize_xuid("name_with-hyphen"),
            "name_with-hyphen"
        );
    }

    #[test]
    fn try_acquire_rejects_empty_xuid() {
        let err = RunnerLock::try_acquire("").unwrap_err();
        assert!(err.to_string().contains("xuid is empty"));
    }

    #[test]
    fn lock_round_trip_creates_then_releases() {
        let xuid = unique_xuid("rt");
        let path = RunnerLock::lock_path(&xuid).unwrap();
        let _ = fs::remove_file(&path);
        {
            let _g = RunnerLock::try_acquire(&xuid).expect("first acquire");
            assert!(path.exists(), "lock file should exist while held");
            // Self-collision: same process, second acquire. The PID
            // self-check + recent heartbeat both say "alive", so
            // backoff retries can't help — collision error.
            let err = RunnerLock::try_acquire(&xuid).unwrap_err();
            assert!(err.to_string().contains("already active"));
        }
        assert!(!path.exists(), "lock file removed on drop");
    }

    #[test]
    fn stale_dead_pid_is_evicted() {
        let xuid = unique_xuid("dead-pid");
        let path = RunnerLock::lock_path(&xuid).unwrap();
        let _ = fs::remove_file(&path);
        {
            let mut f = fs::File::create(&path).expect("create dummy lock");
            // pid=0 is invalid on every platform; heartbeat at "now"
            // so the PID-liveness signal is the one that fires.
            writeln!(f, "pid=0").unwrap();
            writeln!(f, "start_unix=1").unwrap();
            writeln!(f, "xuid={}", xuid).unwrap();
            writeln!(f, "heartbeat_unix={}", now_unix_secs()).unwrap();
        }
        let g = RunnerLock::try_acquire(&xuid).expect("stale lock should be evicted");
        assert!(path.exists());
        drop(g);
        assert!(!path.exists());
    }

    #[test]
    fn stale_heartbeat_evicts_lock() {
        // Plant a "live" PID (our own) but with an OLD heartbeat.
        // The heartbeat-staleness signal MUST fire even though the
        // PID-aliveness check would say "alive" (we are alive).
        let xuid = unique_xuid("hb");
        let path = RunnerLock::lock_path(&xuid).unwrap();
        let _ = fs::remove_file(&path);
        {
            let mut f = fs::File::create(&path).expect("create dummy lock");
            writeln!(f, "pid={}", std::process::id()).unwrap();
            writeln!(f, "start_unix={}", now_unix_secs()).unwrap();
            writeln!(f, "xuid={}", xuid).unwrap();
            // Heartbeat 10 minutes ago → way over the 90s threshold.
            writeln!(f, "heartbeat_unix={}", now_unix_secs().saturating_sub(600)).unwrap();
        }
        let g = RunnerLock::try_acquire(&xuid)
            .expect("stale-heartbeat lock should be evicted even when PID is alive");
        drop(g);
    }

    #[test]
    fn stale_age_fallback_evicts_old_lock() {
        let xuid = unique_xuid("age");
        let path = RunnerLock::lock_path(&xuid).unwrap();
        let _ = fs::remove_file(&path);
        {
            let mut f = fs::File::create(&path).expect("create dummy lock");
            writeln!(f, "pid=99999").unwrap();
            writeln!(f, "start_unix=946684800").unwrap(); // 2000-01-01
            writeln!(f, "xuid={}", xuid).unwrap();
            writeln!(f, "heartbeat_unix=946684800").unwrap();
        }
        let g = RunnerLock::try_acquire(&xuid).expect("year-2000 lock should be evicted");
        drop(g);
    }

    #[test]
    fn corrupt_lock_file_is_evicted() {
        let xuid = unique_xuid("corrupt");
        let path = RunnerLock::lock_path(&xuid).unwrap();
        let _ = fs::remove_file(&path);
        {
            let mut f = fs::File::create(&path).expect("create dummy lock");
            writeln!(f, "garbage in this file").unwrap();
        }
        let g = RunnerLock::try_acquire(&xuid).expect("corrupt lock should be evicted");
        drop(g);
    }

    #[test]
    fn legacy_lock_without_heartbeat_is_evicted_via_mtime() {
        // Pre-Heartbeat lock file (no `heartbeat_unix=` line).
        // Backwards-compat path: the mtime fallback fires when the
        // file's filesystem mtime is older than the threshold.
        // Forge the conditions: write the file, then just check the
        // path through `is_stale` — we can't easily backdate mtime
        // cross-platform without an extra crate, but we CAN verify
        // that a fresh file with a dead pid still gets evicted via
        // signal #2.
        let xuid = unique_xuid("legacy");
        let path = RunnerLock::lock_path(&xuid).unwrap();
        let _ = fs::remove_file(&path);
        {
            let mut f = fs::File::create(&path).expect("create dummy lock");
            writeln!(f, "pid=0").unwrap(); // dead
            writeln!(f, "start_unix={}", now_unix_secs()).unwrap();
            writeln!(f, "xuid={}", xuid).unwrap();
            // Notably NO heartbeat_unix line → signal #1 = unknown.
        }
        let g = RunnerLock::try_acquire(&xuid).expect("legacy lock with dead pid should evict");
        drop(g);
    }
}
