//! Multi-token pool for Xbox Live scans. Holds **every validated
//! XAUTH** we've discovered — whether from memory scan, OAuth login,
//! manual paste, or a previous session — and hands them out
//! round-robin across distinct authenticated XUIDs.
//!
//! # Why pool tokens
//!
//! Xbox Live enforces rate limits per API surface (scan endpoint:
//! 150 req / 300 sec; update endpoint: 100 req / 15 sec). The
//! **bucket key** — what makes two requests count against the same
//! budget — is not publicly documented. Two plausible candidates:
//!
//! - Per authenticated principal (the XUID inside the JWT).
//! - Per raw XAUTH string.
//!
//! This code originally assumed per-XAUTH (which would make holding
//! 10 tokens for one account multiply budget 10×), but that claim
//! was never empirically verified. The observable evidence actually
//! leans the other way: Xbox maintains silent penalty boxes keyed
//! on target XUID that follow a user across every token — implying
//! buckets are identity-scoped, not token-scoped. Standard
//! server-side rate-limit design also keys on principal, not
//! credential.
//!
//! So this implementation **assumes per-principal**: rotation is
//! across distinct XUIDs. N tokens for the same account do NOT
//! multiply budget; they're kept as fallbacks for 401s. What DOES
//! multiply budget is N different accounts (N different XUIDs).
//! The "easy mode" still works: user switches accounts in the Xbox
//! PC app, memory exposes new tokens, we validate + add the new
//! XUID to the rotation automatically.
//!
//! # Token lifecycle
//!
//! ```text
//!  discovered → validated → active ─┐
//!                ▲                   │ any Xbox 401
//!                │                   ▼
//!            revalidate         quarantined
//!            (every 15 min)          │ >12h in quarantine
//!                                    ▼
//!                                 evicted
//! ```
//!
//! - **discovered**: raw XAUTH pattern saw in memory. Never usable.
//! - **validated**: pinged `profile.xboxlive.com/settings?settings=Gamertag`
//!   successfully within the last 15 min. Has `xuid` + `gamertag`.
//! - **active**: used for real scan requests. Counts against its own
//!   throttle gate.
//! - **quarantined**: got a 401 from a real request; kept for 12h so
//!   a user who logs back in with the same refresh token re-activates
//!   the entry instead of re-discovering it from scratch.
//! - **evicted**: removed entirely (dead > 12h).
//!
//! # Persistence
//!
//! Pool saves to `%USERPROFILE%/Documents/XAU/token-pool.json` on
//! every material change. Next launch reloads + revalidates — if a
//! token's still live we skip re-discovering it via memory scan.

// TTL constants are compile-time small (≤ 43200 s). Their `as i64`
// casts can never wrap — clippy's `cast_possible_wrap` lint here would
// force verbose `i64::try_from(...).unwrap_or(i64::MAX)` boilerplate
// at every comparison site with zero safety benefit. Allow at module
// level instead.
#![allow(clippy::cast_possible_wrap)]

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::settings;

/// How stale a validated token can be before we re-ping Xbox.
pub const VALIDATION_TTL: Duration = Duration::from_secs(15 * 60);

/// How long a 401'd token stays in quarantine before we evict it.
/// 12h so a user who refreshes auth keeps their entry alive instead
/// of us re-discovering from scratch.
pub const QUARANTINE_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// How long a `Discovered` entry can sit without validating before
/// we assume it's garbage and evict it. The memory scanner is noisy:
/// `XBL3.0 x=` appears in the Xbox PC app's memory thousands of
/// times in various encodings and buffer fragments, but only a
/// handful of those are real, valid JWTs. If a candidate hasn't
/// passed `validate_token` within 5 min of first discovery, it's
/// almost certainly junk — drop it so the pool doesn't balloon.
pub const DISCOVERED_TTL: Duration = Duration::from_secs(5 * 60);

/// Hard cap on how many `Discovered` (unvalidated) entries the pool
/// keeps at any moment. Protects against a memory-scan run dumping
/// tens of thousands of garbage candidates into the pool before the
/// revalidation loop can sort them out. Validated + Quarantined
/// entries are NOT counted — those are legitimate, bounded by the
/// number of real accounts the user has logged into.
pub const MAX_UNVALIDATED: usize = 24;

/// Hard cap on how many validated tokens we keep for the *same*
/// authenticated XUID. Xbox's rate-limit budget is almost certainly
/// keyed on the authenticated principal (the XUID inside the JWT),
/// not the raw token string — so N tokens for one XUID very likely
/// share one bucket. Keeping a small number of them is still useful
/// as **fallbacks**: if the canonical token gets 401'd (token
/// rotation on Microsoft's end), we swap to a sibling without
/// re-logging-in. 3 is plenty; anything more is noise.
pub const MAX_PER_XUID: usize = 3;

/// State of a single entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenStatus {
    /// Raw pattern found in memory, not yet validated.
    #[default]
    Discovered,
    /// `validate_token` accepted this within `VALIDATION_TTL`.
    Validated,
    /// Got a 401 from a real request; waiting for `QUARANTINE_TTL` to
    /// expire before eviction.
    Quarantined,
}

/// One entry in the pool. Keyed by the full XAUTH string because
/// Xbox treats each distinct token as its own budget bucket.
#[derive(Clone, Serialize, Deserialize)]
pub struct TokenEntry {
    pub xauth: String,
    /// Set after first successful validation.
    #[serde(default)]
    pub xuid: Option<String>,
    #[serde(default)]
    pub gamertag: Option<String>,
    pub status: TokenStatus,
    /// Unix seconds when we first saw this token anywhere.
    pub discovered_at: i64,
    /// Unix seconds of last successful `validate_token`.
    #[serde(default)]
    pub last_validated_at: i64,
    /// Unix seconds of last request fired against this token.
    /// Used for round-robin "pick the least-recently-used valid token".
    #[serde(default)]
    pub last_used_at: i64,
    /// Unix seconds of last 401 (if any). Drives quarantine eviction.
    #[serde(default)]
    pub last_401_at: i64,
    /// Free-text source label for diagnostics.
    /// Examples: "memory:XboxPcApp", "oauth:login", "manual", "settings".
    #[serde(default)]
    pub source: String,
}

// Manual Debug — emits only the masked token. Without this a stray
// `tracing::warn!(?entry, ...)` would log the full XBL3.0 bearer.
impl std::fmt::Debug for TokenEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenEntry")
            .field("xauth", &self.masked())
            .field("xuid", &self.xuid)
            .field("gamertag", &self.gamertag)
            .field("status", &self.status)
            .field("discovered_at", &self.discovered_at)
            .field("last_validated_at", &self.last_validated_at)
            .field("last_used_at", &self.last_used_at)
            .field("last_401_at", &self.last_401_at)
            .field("source", &self.source)
            .finish()
    }
}

impl TokenEntry {
    /// Short masked view of the XAUTH for logs — never print the full
    /// token to disk / stderr.
    #[must_use]
    pub fn masked(&self) -> String {
        self.xauth
            .strip_prefix("XBL3.0 x=")
            .and_then(|rest| rest.split_once(';'))
            .map_or_else(
                || self.xauth.chars().take(24).collect(),
                |(uhs, tok)| {
                    format!(
                        "XBL3.0 x={uhs};{}…({} chars)",
                        tok.chars().take(10).collect::<String>(),
                        tok.len()
                    )
                },
            )
    }

    /// True when this token passed validation within the TTL window.
    #[must_use]
    pub fn is_usable(&self, now_unix: i64) -> bool {
        matches!(self.status, TokenStatus::Validated)
            && now_unix.saturating_sub(self.last_validated_at) <= VALIDATION_TTL.as_secs() as i64
    }

    /// True when we should `validate_token` again to refresh state.
    #[must_use]
    pub fn needs_revalidation(&self, now_unix: i64) -> bool {
        match self.status {
            TokenStatus::Discovered => true,
            TokenStatus::Validated => {
                now_unix.saturating_sub(self.last_validated_at) > VALIDATION_TTL.as_secs() as i64
            }
            TokenStatus::Quarantined => false,
        }
    }

    /// True when an entry has aged out of its status window.
    ///
    /// - `Quarantined` → evict after `QUARANTINE_TTL` (12h).
    /// - `Discovered` → evict after `DISCOVERED_TTL` (5 min) — if a
    ///   candidate hasn't validated by then it's memory-scan junk.
    /// - `Validated` → never evict on age alone (bumped by
    ///   revalidation / kept for fallback rotation).
    #[must_use]
    pub fn should_evict(&self, now_unix: i64) -> bool {
        match self.status {
            TokenStatus::Quarantined => {
                now_unix.saturating_sub(self.last_401_at) > QUARANTINE_TTL.as_secs() as i64
            }
            TokenStatus::Discovered => {
                now_unix.saturating_sub(self.discovered_at) > DISCOVERED_TTL.as_secs() as i64
            }
            TokenStatus::Validated => false,
        }
    }
}

/// Reason a candidate failed the shape test — used for tracing so we
/// can see *why* a token got rejected without logging the token itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeReject {
    Ok,
    Empty,
    TooShort,
    TooLong,
    MissingPrefix,
    MissingSemicolon,
    UhsEmpty,
    UhsNonDigit,
    UhsTooLong,
    JwtWrongSegmentCount,
    JwtSegmentEmpty,
    JwtSegmentNonBase64Url,
    ContainsWhitespace,
}

/// Shape-test an `XBL3.0` XAUTH string before letting it into the
/// pool. Returns the reason for the decision so tracing can log
/// *why* a candidate was rejected without leaking the token body.
///
/// A real XAUTH is `XBL3.0 x=<uhs>;<jwt>` where `jwt` has 3
/// base64url segments. We're permissive on sizes (real tokens vary
/// from ~200 to ~2500 chars) but strict on structure — the
/// segment-count + base64url check is what kills memory-scan junk
/// (partial copies, raw bytes after a format string, etc.).
#[must_use]
pub fn classify_xauth_shape(xauth: &str) -> ShapeReject {
    if xauth.is_empty() {
        return ShapeReject::Empty;
    }
    // A real XAUTH is always well over 80 chars (prefix + uhs + ";"
    // + header + "." + payload + "." + sig). Smaller strings are
    // truncations or dumps of unrelated short strings.
    if xauth.len() < 80 {
        return ShapeReject::TooShort;
    }
    // 8 KiB is an absurd upper bound for a JWT — past this we've
    // swallowed adjacent memory or concatenated multiple tokens.
    if xauth.len() > 8192 {
        return ShapeReject::TooLong;
    }
    let Some(rest) = xauth.strip_prefix("XBL3.0 x=") else {
        return ShapeReject::MissingPrefix;
    };
    // Real tokens are printable ASCII only. Whitespace / controls
    // mean we grabbed raw memory bytes — reject early.
    if rest.bytes().any(|b| !(0x21..=0x7E).contains(&b)) {
        return ShapeReject::ContainsWhitespace;
    }
    let Some((uhs, jwt)) = rest.split_once(';') else {
        return ShapeReject::MissingSemicolon;
    };
    if uhs.is_empty() {
        return ShapeReject::UhsEmpty;
    }
    if uhs.len() > 32 {
        return ShapeReject::UhsTooLong;
    }
    // Xbox user hashes are decimal digits. The C# upstream and
    // memory/win.rs both rely on this assumption.
    if !uhs.bytes().all(|b| b.is_ascii_digit()) {
        return ShapeReject::UhsNonDigit;
    }
    // JWT part: Xbox Live's real XSTS tokens are JWE (JSON Web
    // Encryption) with **5** dot-separated segments
    // (`header.encrypted_key.iv.ciphertext.authtag`), NOT the
    // 3-segment JWS (`header.payload.signature`) form you see in
    // most web tutorials. Accept either — matching only 3 segments
    // was the silent bug that made the whole pool stay empty for
    // real signed-in users, because every live Xbox token has
    // `alg: RSA-OAEP` with a JWE body.
    let segs: Vec<&str> = jwt.split('.').collect();
    if segs.len() != 3 && segs.len() != 5 {
        return ShapeReject::JwtWrongSegmentCount;
    }
    for s in &segs {
        if s.is_empty() {
            return ShapeReject::JwtSegmentEmpty;
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'=')
        {
            return ShapeReject::JwtSegmentNonBase64Url;
        }
    }
    ShapeReject::Ok
}

/// Convenience boolean wrapper around `classify_xauth_shape`.
#[must_use]
pub fn looks_like_real_xauth(xauth: &str) -> bool {
    classify_xauth_shape(xauth) == ShapeReject::Ok
}

/// On-disk snapshot of the pool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenPoolSnapshot {
    pub version: u32,
    pub tokens: Vec<TokenEntry>,
}

/// Mutation-safe handle shared across tasks. All state lives behind
/// `RwLock`. Clone is cheap (Arc).
#[derive(Clone, Default)]
pub struct TokenPool {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Keyed by XAUTH string — same XAUTH found from multiple sources
    /// merges into one entry.
    tokens: RwLock<HashMap<String, TokenEntry>>,
    /// Atomic round-robin cursor. `pick_for_request` increments this
    /// and returns the entry at `cursor % valid_count`.
    cursor: Mutex<usize>,
    /// Serializes `save_to_disk` callers so two concurrent writers
    /// (background maintenance + an IPC handler + a 401 handler)
    /// can't race on the temp-file + rename dance and produce a
    /// half-written `token-pool.json`. `RwLock` on `tokens` alone
    /// didn't cover this: both saves can take read locks
    /// simultaneously, snapshot the data, then race `fs::rename`.
    save_mutex: Mutex<()>,
}

impl TokenPool {
    /// Empty pool, no persistence yet. Call `load_from_disk` to seed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the persisted pool from `%USERPROFILE%/Documents/XAU/token-pool.json`.
    /// Missing file = empty pool; malformed file = empty pool + warn.
    #[must_use]
    pub fn load_from_disk() -> Self {
        let pool = Self::new();
        let Ok(path) = pool_path() else {
            return pool;
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            return pool;
        };
        match serde_json::from_str::<TokenPoolSnapshot>(&raw) {
            Ok(snap) => {
                let now = now_unix();
                let loaded_raw = snap.tokens.len();
                let mut map = pool
                    .inner
                    .tokens
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut rejected_shape = 0usize;
                let mut rejected_ttl = 0usize;
                for tok in snap.tokens {
                    // Old pool files (before the shape filter existed)
                    // may contain thousands of junk "tokens" — purge
                    // them here so the pool starts clean even on
                    // upgrade without needing the user to manually
                    // delete token-pool.json.
                    if !looks_like_real_xauth(&tok.xauth) {
                        rejected_shape += 1;
                        continue;
                    }
                    // Apply the same TTL rules that `evict_dead` would
                    // on a fresh load — no point resurrecting entries
                    // that were due for eviction anyway.
                    if tok.should_evict(now) {
                        rejected_ttl += 1;
                        continue;
                    }
                    map.insert(tok.xauth.clone(), tok);
                }
                tracing::info!(
                    loaded_raw,
                    kept = map.len(),
                    rejected_shape,
                    rejected_ttl,
                    path = %path.display(),
                    "token pool loaded from disk"
                );
                // Persist the cleaned-up pool so next launch doesn't
                // pay the same cost.
                drop(map);
                if rejected_shape > 0 || rejected_ttl > 0 {
                    pool.save_to_disk();
                }
            }
            Err(e) => {
                tracing::warn!(?e, path = %path.display(), "token pool file malformed, starting empty");
            }
        }
        pool
    }

    /// Persist the current pool to disk. No-op if the path can't be
    /// resolved (CI/headless). Errors log-only.
    /// Persist the pool atomically. Writes to a sibling `*.tmp` file
    /// first, then `fs::rename` into place — so a crash mid-write
    /// leaves the OLD file intact rather than a truncated JSON that
    /// `load_from_disk` would log-and-drop on next launch.
    ///
    /// Guarded by `save_mutex` so concurrent savers can't interleave
    /// their temp-file writes and rename each other's temps.
    pub fn save_to_disk(&self) {
        let _guard = self
            .inner
            .save_mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = TokenPoolSnapshot {
            version: 1,
            tokens: self
                .inner
                .tokens
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .cloned()
                .collect(),
        };
        let Ok(path) = pool_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let json = match serde_json::to_string_pretty(&snap) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, "token pool serialize failed");
                return;
            }
        };
        // Temp file sibling of the target, so `fs::rename` stays on
        // the same filesystem (Windows' `MoveFileEx` is only atomic
        // within one volume anyway).
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = fs::write(&tmp, &json) {
            tracing::warn!(?e, path = %tmp.display(), "token pool temp-write failed");
            return;
        }
        // BUG HUNTER M-tokenpool-truncate: previously a rename
        // failure fell back to `fs::write(&path, &json)` which
        // truncates the existing file before writing. A subsequent
        // crash mid-fallback-write left `token-pool.json` empty/
        // half-written and on next launch the user lost every
        // validated token they collected over hours of work. Fix:
        // retry the rename in a short loop (Windows file-in-use is
        // typically a tiny window). If it still fails, KEEP the
        // good copy on disk and leak the temp file — losing one
        // save cycle is far cheaper than corrupting the canonical
        // pool file.
        let mut last_err: Option<std::io::Error> = None;
        for attempt in 0..6 {
            match fs::rename(&tmp, &path) {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    // Exponential-ish backoff: 50ms, 100ms, 200ms,
                    // 400ms, 800ms. Total worst-case ~1.5s.
                    std::thread::sleep(Duration::from_millis(50u64 << attempt.min(4)));
                }
            }
        }
        if let Some(e) = last_err {
            tracing::warn!(
                ?e,
                path = %path.display(),
                "token pool atomic rename failed after retries — cleaning up temp file with secrets, canonical file unchanged",
            );
            // BUG HUNTER M-tokenpool-tmp-leak: previously this branch
            // intentionally LEAKED `token-pool.json.tmp` on disk —
            // and that file contains every plaintext XAUTH from the
            // pool. A user with OneDrive Documents sync would see
            // those tokens uploaded plaintext. Always remove the
            // temp on terminal failure; next save cycle re-creates
            // it from scratch.
            let _ = fs::remove_file(&tmp);
        } else {
            // Rename succeeded; if a stale tmp somehow remained
            // (shouldn't, rename consumes it), clean it up.
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Insert a newly-discovered raw XAUTH. Returns `true` when the
    /// entry was genuinely new. Silently rejects junk:
    ///
    /// - Empty / whitespace.
    /// - Strings that don't match the structural XAUTH shape
    ///   (see [`looks_like_real_xauth`]).
    /// - New entries that would push the unvalidated count past
    ///   [`MAX_UNVALIDATED`] — stops a flood of memory-scan garbage
    ///   from crowding out real tokens.
    ///
    /// Same token seen twice is a no-op on state — we refresh
    /// `discovered_at` and append the new source label so the UI can
    /// show "first memory, now oauth too".
    pub fn discover(&self, xauth: String, source: &str) -> bool {
        if xauth.trim().is_empty() {
            return false;
        }
        let shape = classify_xauth_shape(&xauth);
        if shape != ShapeReject::Ok {
            // `info` not `debug` so users can see in logs WHY a
            // token was rejected when the pool stays empty. Never
            // logs the token body, just the length + reject reason.
            tracing::info!(
                len = xauth.len(),
                reason = ?shape,
                source,
                "pool: rejected candidate with non-XAUTH shape"
            );
            return false;
        }
        let now = now_unix();
        let mut map = self
            .inner
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = map.get_mut(&xauth) {
            // Same token seen again — don't overwrite validation
            // state, just tag it fresh.
            existing.discovered_at = existing.discovered_at.max(now);
            if !source.is_empty() && existing.source != source {
                existing.source = format!("{};{source}", existing.source);
            }
            return false;
        }
        // Cap unvalidated entries. Count tokens currently in Discovered
        // state; if we're already at the cap, drop the oldest discovered
        // entry to make room (or refuse if the incoming one has no better
        // claim than the existing junk).
        let unvalidated_count = map
            .values()
            .filter(|e| e.status == TokenStatus::Discovered)
            .count();
        if unvalidated_count >= MAX_UNVALIDATED {
            // Evict the oldest Discovered entry to make space.
            let victim = map
                .values()
                .filter(|e| e.status == TokenStatus::Discovered)
                .min_by_key(|e| e.discovered_at)
                .map(|e| e.xauth.clone());
            if let Some(v) = victim {
                map.remove(&v);
                tracing::debug!(
                    "pool: unvalidated cap {} reached; evicted oldest to admit new candidate",
                    MAX_UNVALIDATED
                );
            } else {
                // Shouldn't happen (count was >= cap), but be safe.
                return false;
            }
        }
        map.insert(
            xauth.clone(),
            TokenEntry {
                xauth,
                xuid: None,
                gamertag: None,
                status: TokenStatus::Discovered,
                discovered_at: now,
                last_validated_at: 0,
                last_used_at: 0,
                last_401_at: 0,
                source: source.to_string(),
            },
        );
        true
    }

    /// Immediately drop every entry whose status is `Discovered`.
    /// The "Prune dead" button in the GUI calls this when the user
    /// wants to clean up after a bad scan without waiting for
    /// `DISCOVERED_TTL`. Returns the number of entries evicted.
    pub fn prune_unvalidated(&self) -> usize {
        let mut map = self
            .inner
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = map.len();
        map.retain(|_, e| e.status != TokenStatus::Discovered);
        before - map.len()
    }

    /// Mark a token as validated with the given profile identity.
    /// No-op if the token isn't in the pool.
    ///
    /// After promoting, enforces [`MAX_PER_XUID`] — if this XUID now
    /// has more validated tokens than the cap, the oldest (by
    /// `last_validated_at`) siblings get evicted. This keeps the
    /// per-identity fallback set small without losing the freshest
    /// working token.
    ///
    /// Also **always** overwrites `xuid` / `gamertag` when the caller
    /// provides them — earlier versions only set them when the
    /// existing field was `None`, which meant a token that got
    /// rotated to a different Microsoft account (rare but possible
    /// when Xbox issues a refresh token re-binding) kept the stale
    /// identity. Re-writing is the safer default.
    pub fn mark_validated(&self, xauth: &str, xuid: Option<String>, gamertag: Option<String>) {
        let now = now_unix();
        // Wave-6 audit i18n-MED: previously the upstream-supplied
        // gamertag was assigned verbatim without going through
        // `Gamertag::try_from`. A poisoned profile response (or a
        // corrupted token-pool.json) carrying BiDi controls,
        // 100KB-long values, embedded newlines, etc. would round-
        // trip into the GUI and forensic logs unchecked. Validate
        // first; on rejection, drop to None so the pool entry
        // stores no gamertag at all rather than poisoned bytes.
        let validated_gamertag: Option<String> =
            gamertag.and_then(|gt| match crate::types::Gamertag::try_from(gt.as_str()) {
                Ok(g) => Some(g.into()),
                Err(e) => {
                    tracing::warn!(
                        err = %e,
                        len = gt.len(),
                        "rejecting upstream gamertag — fails Gamertag validation; \
                         token will be stored without a gamertag"
                    );
                    None
                }
            });
        let mut map = self
            .inner
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let new_xuid = xuid.clone();
        if let Some(e) = map.get_mut(xauth) {
            e.status = TokenStatus::Validated;
            e.last_validated_at = now;
            if let Some(x) = xuid {
                e.xuid = Some(x);
            }
            if let Some(gt) = validated_gamertag {
                e.gamertag = Some(gt);
            }
        } else {
            return;
        }
        // Cap per-XUID fallback count.
        if let Some(x) = new_xuid {
            let mut siblings: Vec<(String, i64)> = map
                .values()
                .filter(|e| {
                    e.status == TokenStatus::Validated && e.xuid.as_deref() == Some(x.as_str())
                })
                .map(|e| (e.xauth.clone(), e.last_validated_at))
                .collect();
            if siblings.len() > MAX_PER_XUID {
                // BUG HUNTER pool-evict-determinism: keep the freshest
                // MAX_PER_XUID, but make the tie-break DETERMINISTIC and
                // never evict the token we just validated. The old
                // `sort_by(|a, b| b.1.cmp(&a.1))` compared only
                // `last_validated_at`; under concurrent `revalidate_all`
                // several siblings share the same second, and the residual
                // order was then `HashMap::values()` iteration order
                // (nondeterministic) — so the just-promoted `xauth` could
                // land in the evicted tail while an equally-fresh older
                // sibling survived. Tie-break: (1) newest first, then
                // (2) the current `xauth` wins, then (3) by `xauth` string
                // for a stable, reproducible order.
                siblings.sort_by(|a, b| {
                    b.1.cmp(&a.1)
                        .then_with(|| (b.0.as_str() == xauth).cmp(&(a.0.as_str() == xauth)))
                        .then_with(|| a.0.cmp(&b.0))
                });
                for (xa, _) in siblings.into_iter().skip(MAX_PER_XUID) {
                    map.remove(&xa);
                }
                tracing::debug!(
                    xuid = %x,
                    cap = MAX_PER_XUID,
                    "pool: pruned excess validated tokens for same xuid"
                );
            }
        }
    }

    /// Mark a token as quarantined (got a 401). Kept in pool for
    /// `QUARANTINE_TTL` so a subsequent refresh / re-login reactivates
    /// instead of re-discovering.
    pub fn mark_quarantined(&self, xauth: &str) {
        let now = now_unix();
        let mut map = self
            .inner
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(e) = map.get_mut(xauth) {
            e.status = TokenStatus::Quarantined;
            e.last_401_at = now;
        }
    }

    /// Drop tokens whose quarantine has expired. Called by the
    /// background revalidation task.
    pub fn evict_dead(&self) -> usize {
        let now = now_unix();
        let mut map = self
            .inner
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = map.len();
        map.retain(|_, e| !e.should_evict(now));
        before - map.len()
    }

    /// Return every entry for diagnostics (GUI pool view). Clones so
    /// caller doesn't hold the lock.
    #[must_use]
    pub fn snapshot(&self) -> Vec<TokenEntry> {
        self.inner
            .tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Every currently-usable token, ordered by `last_used_at` ASC
    /// (least-recently-used first). Good for distributing N requests
    /// across K tokens.
    #[must_use]
    pub fn usable_tokens(&self) -> Vec<TokenEntry> {
        let now = now_unix();
        let mut v: Vec<TokenEntry> = self
            .inner
            .tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|e| e.is_usable(now))
            .cloned()
            .collect();
        v.sort_by_key(|e| e.last_used_at);
        v
    }

    /// Count of usable tokens.
    #[must_use]
    pub fn usable_count(&self) -> usize {
        self.usable_tokens().len()
    }

    /// Pick the next token for a request — round-robins across
    /// **distinct authenticated XUIDs**, not across raw token strings.
    ///
    /// Xbox Live's rate-limit policy isn't publicly documented, but
    /// the observable behavior (cross-token target-XUID penalty
    /// boxes, standard server-side rate-limit design) strongly
    /// suggests buckets are keyed on the authenticated principal
    /// (the XUID embedded in the JWT) rather than on the raw token
    /// string. That means N tokens for the SAME XUID almost
    /// certainly share ONE bucket — they don't multiply budget.
    ///
    /// What multiplies budget is multiple **different** accounts
    /// (different XUIDs), because those are distinct principals.
    /// This picker rotates round-robin across distinct XUIDs, and
    /// for each XUID always picks the freshest-validated token.
    /// Any sibling tokens for the same XUID stay in the pool as
    /// silent fallbacks — if the canonical one gets 401'd, the pool
    /// promotes a sibling on the next call.
    ///
    /// Returns `None` when the pool has no usable tokens.
    pub fn pick_for_request(&self) -> Option<TokenEntry> {
        let usable = self.usable_tokens();
        if usable.is_empty() {
            return None;
        }
        // Group by xuid. For each xuid, keep the entry with the most
        // recent `last_validated_at`. Tokens with no xuid (shouldn't
        // happen post-validation, but defensively) land in an "unknown"
        // bucket under the masked-prefix key so they still rotate.
        let mut by_xuid: HashMap<String, TokenEntry> = HashMap::new();
        for e in usable {
            let key = e
                .xuid
                .clone()
                .unwrap_or_else(|| format!("_anon:{}", e.masked()));
            match by_xuid.get(&key) {
                Some(cur) if cur.last_validated_at >= e.last_validated_at => {}
                _ => {
                    by_xuid.insert(key, e);
                }
            }
        }
        let mut canonical: Vec<TokenEntry> = by_xuid.into_values().collect();
        if canonical.is_empty() {
            return None;
        }
        // Stable order → reproducible rotation. Sort by xuid so the
        // cursor lands on the same bucket each pass regardless of
        // HashMap iteration order.
        canonical.sort_by(|a, b| a.xuid.cmp(&b.xuid).then(a.xauth.cmp(&b.xauth)));
        let mut cursor = self
            .inner
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = *cursor % canonical.len();
        *cursor = cursor.wrapping_add(1);
        drop(cursor);
        let picked = canonical[idx].clone();
        // Stamp last_used_at for LRU.
        if let Some(e) = self
            .inner
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&picked.xauth)
        {
            e.last_used_at = now_unix();
        }
        Some(picked)
    }

    /// Entries that need revalidation (discovered, or validated-stale).
    /// Caller runs `validate_token` on each and reports back via
    /// `mark_validated` / `mark_quarantined`.
    #[must_use]
    pub fn tokens_needing_revalidation(&self) -> Vec<TokenEntry> {
        let now = now_unix();
        self.inner
            .tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|e| e.needs_revalidation(now))
            .cloned()
            .collect()
    }

    /// Every DISTINCT xuid the pool has seen (validated tokens only).
    /// Lets the UI say "pool has 3 accounts — Alice, Bob, Carol".
    #[must_use]
    pub fn distinct_xuids(&self) -> Vec<String> {
        let mut set = std::collections::HashSet::new();
        for e in self
            .inner
            .tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            if e.status == TokenStatus::Validated {
                if let Some(x) = &e.xuid {
                    set.insert(x.clone());
                }
            }
        }
        set.into_iter().collect()
    }
}

/// Path to the persistent pool file. Lives in `%LOCALAPPDATA%/XAU/`
/// (NOT the OneDrive-synced Documents/XAU) because it holds validated
/// XBL3.0 bearer tokens; `credential_path` migrates an existing file
/// out of the legacy location on first access. Audit: plaintext-creds-
/// in-onedrive-synced-documents.
fn pool_path() -> Result<PathBuf> {
    settings::credential_path("token-pool.json")
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

// ==========================================================================
//  DISCOVERY + VALIDATION DRIVER
// ==========================================================================

/// Merge any XAUTHs visible in process memory right now into the
/// pool — but **validate first, store later**.
///
/// The naive version (store every shape-matching candidate as
/// `Discovered`, let `revalidate_all` sort them out) turns out to
/// be a disaster: the Xbox PC app keeps hundreds of token fragments
/// in memory (partial buffers, different relying parties, stale
/// refreshes, log-format templates), and even with the shape
/// filter active we'd still end up with dozens of `Discovered`
/// entries per cycle that the GUI shows as confusing "(unvalidated)"
/// rows.
///
/// This version:
/// 1. Runs `extract_all_candidates(true)` as before.
/// 2. Drops candidates that fail the structural shape test.
/// 3. Drops candidates already known to the pool.
/// 4. Pings `profile.xboxlive.com/users/me` for each new candidate
///    (bounded concurrency = 6), discarding anything Xbox rejects.
/// 5. Inserts ONLY the survivors, already in the `Validated` state
///    with identity populated.
///
/// Net effect: a memory scan that finds 3000 raw byte matches adds
/// 0–2 pool entries — the REAL tokens for the currently-signed-in
/// Xbox account(s), nothing else. The UI stays clean and the
/// revalidation queue isn't buried in noise.
///
/// Returns the count of genuinely-new validated entries added.
pub async fn refresh_from_memory(pool: &TokenPool) -> Result<usize> {
    let candidates = match crate::memory::extract_all_candidates(true) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(?e, "memory scan failed (no Xbox app running?)");
            return Ok(0);
        }
    };
    // Step 1: shape filter + already-known dedup.
    let existing: std::collections::HashSet<String> = pool
        .inner
        .tokens
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .cloned()
        .collect();
    let to_probe: Vec<String> = candidates
        .into_iter()
        .map(|c| c.token)
        .filter(|t| looks_like_real_xauth(t) && !existing.contains(t))
        .collect();
    if to_probe.is_empty() {
        return Ok(0);
    }
    // Step 2: validate each surviving candidate against Xbox. Only
    // the ones Xbox accepts go into the pool.
    let sem = Arc::new(tokio::sync::Semaphore::new(6));
    let mut joins: tokio::task::JoinSet<(String, Option<crate::xbox::ValidatedProfile>)> =
        tokio::task::JoinSet::new();
    for xauth in to_probe {
        let permit = sem.clone();
        joins.spawn(async move {
            let _p = permit.acquire_owned().await.ok();
            let profile = crate::xbox::validate_token(&xauth).await.ok().flatten();
            (xauth, profile)
        });
    }
    let mut added = 0usize;
    let mut touched = 0usize;
    // Use explicit match arms instead of `while let Some(Ok(...))` so a
    // panicked worker (returned as `Err(JoinError)`) doesn't break us
    // out of the loop and silently abandon the remaining validations.
    // See audit bug #16.
    loop {
        match joins.join_next().await {
            None => break,
            Some(Ok((xauth, profile))) => {
                if let Some(p) = profile {
                    // Commit as Validated — unconditionally, even if
                    // the entry already existed as Discovered or
                    // Quarantined. Previously we gated `mark_validated`
                    // behind `was_new`, meaning a memory-scan hit that
                    // re-confirmed an existing (possibly-quarantined)
                    // token stayed stuck in its old state. See audit
                    // bug #17.
                    let was_new = pool.discover(xauth.clone(), "memory");
                    pool.mark_validated(&xauth, p.xuid, p.gamertag);
                    if was_new {
                        added += 1;
                    }
                    touched += 1;
                }
                // Else: Xbox rejected — don't pollute. Junk stays out.
            }
            Some(Err(e)) => {
                tracing::warn!(?e, "memory-scan validation worker panicked — continuing");
            }
        }
    }
    if touched > 0 {
        tracing::info!(
            added,
            touched,
            "token pool memory scan: validated (added={added}, rebound={})",
            touched.saturating_sub(added)
        );
        pool.save_to_disk();
    }
    Ok(added)
}

/// Revalidate everyone that's stale or unchecked. Fires `validate_token`
/// concurrently with a modest bound (4 at a time — profile.xboxlive.com
/// is light and 4 parallel is plenty). Updates pool state.
pub async fn revalidate_all(pool: &TokenPool) -> usize {
    let pending = pool.tokens_needing_revalidation();
    if pending.is_empty() {
        return 0;
    }
    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    let mut joins: tokio::task::JoinSet<(String, Result<Option<crate::xbox::ValidatedProfile>>)> =
        tokio::task::JoinSet::new();
    for e in pending {
        let xauth = e.xauth.clone();
        let permit = sem.clone();
        joins.spawn(async move {
            let _p = permit.acquire_owned().await.ok();
            let profile = crate::xbox::validate_token(&xauth).await;
            (xauth, profile)
        });
    }
    let mut updated = 0usize;
    while let Some(joined) = joins.join_next().await {
        match joined {
            Ok((xauth, Ok(Some(p)))) => {
                pool.mark_validated(&xauth, p.xuid, p.gamertag);
                updated += 1;
            }
            Ok((xauth, Ok(None))) => {
                // validate_token returned None = Xbox rejected.
                // Treat as quarantined (401-like).
                pool.mark_quarantined(&xauth);
            }
            Ok((xauth, Err(e))) => {
                // BUG HUNTER M-token-log: previously logged
                // `xauth_prefix = first 18 chars` which on the
                // `XBL3.0 x=<uhs>;<token>` format leaks the
                // user-hash (a stable identifier that correlates
                // sessions across log dumps). Use the masked()
                // helper which redacts both ends of the token to
                // length-only info.
                let _ = xauth; // suppress unused on the branch — we no longer log it
                tracing::warn!(?e, "token revalidation failed without quarantining");
            }
            Err(e) => {
                tracing::warn!(?e, "token revalidation worker panicked");
            }
        }
    }
    pool.evict_dead();
    pool.save_to_disk();
    tracing::info!(
        updated,
        usable = pool.usable_count(),
        "token pool revalidation pass complete"
    );
    updated
}

/// Notify the pool that a real Xbox request returned 401 Unauthorized
/// for the given XAUTH. Quarantines immediately so subsequent
/// `pick_for_request` calls skip it.
pub fn report_401(pool: &TokenPool, xauth: &str) {
    pool.mark_quarantined(xauth);
    pool.save_to_disk();
    tracing::warn!("pool entry quarantined (401 from live request)");
}

/// Provide the pool with an externally-known token (e.g. freshly
/// minted via OAuth login) plus its known identity. Skips the
/// revalidation round-trip for the first use.
pub fn install_fresh_oauth(
    pool: &TokenPool,
    xauth: &str,
    xuid: Option<String>,
    gamertag: Option<String>,
) {
    // Reject obvious junk silently so a bogus xauth (e.g. a stale
    // clipboard paste) doesn't pollute the pool. Log the decision
    // so the user can diagnose "why didn't my login show up?" from
    // the console without us logging the raw token.
    let shape = classify_xauth_shape(xauth);
    if shape != ShapeReject::Ok {
        tracing::warn!(
            len = xauth.len(),
            reason = ?shape,
            "install_fresh_oauth: refusing to register token — shape invalid"
        );
        return;
    }
    // Capture the flags BEFORE moving into `mark_validated` so we
    // can still log them afterwards without a clone.
    let had_xuid = xuid.is_some();
    let had_gamertag = gamertag.is_some();
    let was_new = pool.discover(xauth.to_string(), "oauth");
    pool.mark_validated(xauth, xuid, gamertag);
    pool.save_to_disk();
    tracing::info!(
        was_new,
        had_xuid,
        had_gamertag,
        pool_size = pool.snapshot().len(),
        usable = pool.usable_count(),
        "install_fresh_oauth: pool updated"
    );
}

// ==========================================================================
//  TESTS
// ==========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a shape-valid XAUTH for tests. Passes `looks_like_real_xauth`
    /// by construction — sizes picked inside each segment's allowed
    /// band AND total string length above 400 chars. The `n`
    /// parameter makes each call produce a DIFFERENT string so tests
    /// that need several distinct pool entries can just call
    /// `fake_xauth(0)..fake_xauth(k)`.
    fn fake_xauth(n: usize) -> String {
        let uhs = format!("{:016}", 1_000_000_000_000_000_u64 + n as u64);
        let header = "a".repeat(100);
        let payload = format!("{}{:06}", "b".repeat(1000), n);
        let sig = "c".repeat(200);
        format!("XBL3.0 x={uhs};{header}.{payload}.{sig}")
    }

    #[test]
    fn looks_like_real_xauth_accepts_fake() {
        assert!(looks_like_real_xauth(&fake_xauth(0)));
    }

    #[test]
    fn looks_like_real_xauth_accepts_real_jwe_token() {
        // The shape every production Xbox XSTS token has: `XBL3.0 x=`,
        // a decimal user hash, `;`, then a **5**-segment JWE
        // (header.encrypted_key.iv.ciphertext.authtag) — not the
        // 3-segment JWS most tutorials show. Matching only 3 segments
        // was the silent bug that kept the pool empty for every real
        // signed-in user, so this shape is worth pinning.
        //
        // Synthetic, and deliberately so. This used to `include_str!` a
        // captured token from `tests/fixtures/`, which compiled a live
        // credential into the test binary, broke `cargo test` on any
        // clone that lacked the untracked file, and left a path inside
        // the repo begging for a real token to be dropped into it. The
        // assertion is about shape; shape is all it needs.
        let token = "XBL3.0 x=1234567890123456;\
             eyJhbGciOiJSU0EtT0FFUCIsImVuYyI6IkEyNTZHQ00ifQ.\
             T0ZBS0VfRU5DUllQVEVEX0tFWV9TRUdNRU5UX0ZPUl9TSEFQRV9URVNUUw.\
             SVZfU0VHTUVOVF8xMjM.\
             Q0lQSEVSVEVYVF9TRUdNRU5UX1RIQVRfSVNfUExFTlRZX0xPTkdfRU5PVUdI.\
             QVVUSF9UQUdfU0VHTUVOVA";
        assert!(
            looks_like_real_xauth(token),
            "rejected a correctly shaped Xbox XSTS token — reason: {:?}",
            classify_xauth_shape(token)
        );
    }

    #[test]
    fn looks_like_real_xauth_accepts_3_segment_jws() {
        // Older / some edge flows produce 3-segment JWS. Keep
        // accepting both shapes so we don't regress recognition
        // of whichever form the Xbox auth path returns.
        let t = fake_xauth(0);
        assert!(looks_like_real_xauth(&t));
    }

    #[test]
    fn looks_like_real_xauth_accepts_5_segment_jwe() {
        // Mimic a JWE shape by hand (5 base64url segments).
        let uhs = "13786090807936252401";
        let seg1 = "a".repeat(100);
        let seg2 = "b".repeat(200);
        let seg3 = "c".repeat(22); // JWE IV is typically 16 bytes → 22 chars b64
        let seg4 = "d".repeat(800);
        let seg5 = "e".repeat(22);
        let xauth = format!("XBL3.0 x={uhs};{seg1}.{seg2}.{seg3}.{seg4}.{seg5}");
        assert!(
            looks_like_real_xauth(&xauth),
            "rejected a synthetic 5-segment JWE token — reason: {:?}",
            classify_xauth_shape(&xauth)
        );
    }

    #[test]
    fn looks_like_real_xauth_rejects_garbage() {
        // Wrong prefix
        assert!(!looks_like_real_xauth("garbage"));
        assert!(!looks_like_real_xauth("Bearer foo.bar.baz"));
        // Short UHS
        assert!(!looks_like_real_xauth("XBL3.0 x=123;a.b.c"));
        // Non-digit UHS
        assert!(!looks_like_real_xauth(
            "XBL3.0 x=abcdefghijklmnop;aaaa.bbbb.cccc"
        ));
        // Wrong number of JWT segments
        let uhs = "1234567890123456";
        assert!(!looks_like_real_xauth(&format!("XBL3.0 x={uhs};onlyone")));
        assert!(!looks_like_real_xauth(&format!("XBL3.0 x={uhs};two.only")));
        // Payload too short
        assert!(!looks_like_real_xauth(&format!(
            "XBL3.0 x={uhs};aaaaaaaaaaaaaaaaaaaaaaaa.bb.cccccccccccccccccccccccc"
        )));
        // Non-base64url chars in a segment
        assert!(!looks_like_real_xauth(&format!(
            "XBL3.0 x={uhs};aaa aaa.{}.cccccccccccccccccccccc",
            "b".repeat(200)
        )));
    }

    #[test]
    fn looks_like_real_xauth_rejects_byte_fragment_junk() {
        // Representative of what the memory scanner picks up in the
        // Xbox app — XBL3.0 x= prefix is there but the following
        // bytes are garbage (e.g. raw memory following a log format
        // string). 99%+ of such matches should be rejected.
        assert!(!looks_like_real_xauth("XBL3.0 x=;"));
        assert!(!looks_like_real_xauth(
            "XBL3.0 x=2535412146263424;\x00\x00\x00\x00"
        ));
        let long_junk = "XBL3.0 x=2535412146263424;".to_string() + &"\x01".repeat(500);
        assert!(!looks_like_real_xauth(&long_junk));
    }

    #[test]
    fn discover_inserts_new_entry() {
        let pool = TokenPool::new();
        assert!(pool.discover(fake_xauth(0), "test"));
        assert_eq!(pool.snapshot().len(), 1);
    }

    #[test]
    fn discover_rejects_non_xauth_shape() {
        let pool = TokenPool::new();
        assert!(!pool.discover("XBL3.0 x=a;b".into(), "test"));
        assert!(!pool.discover("nope".into(), "test"));
        assert_eq!(pool.snapshot().len(), 0);
    }

    #[test]
    fn discover_deduplicates_exact_same_xauth() {
        let pool = TokenPool::new();
        let t = fake_xauth(0);
        assert!(pool.discover(t.clone(), "src1"));
        assert!(!pool.discover(t, "src2"));
        assert_eq!(pool.snapshot().len(), 1);
    }

    #[test]
    fn discover_merges_sources() {
        let pool = TokenPool::new();
        let t = fake_xauth(0);
        pool.discover(t.clone(), "memory");
        pool.discover(t, "manual");
        let snap = pool.snapshot();
        assert!(snap[0].source.contains("memory"));
        assert!(snap[0].source.contains("manual"));
    }

    #[test]
    fn discover_ignores_empty() {
        let pool = TokenPool::new();
        assert!(!pool.discover(String::new(), "test"));
        assert_eq!(pool.snapshot().len(), 0);
    }

    #[test]
    fn discover_caps_unvalidated_entries() {
        let pool = TokenPool::new();
        // Insert up to the cap.
        for i in 0..MAX_UNVALIDATED {
            assert!(pool.discover(fake_xauth(i), "test"));
        }
        assert_eq!(pool.snapshot().len(), MAX_UNVALIDATED);
        // Next insert should still succeed (it evicts the oldest
        // discovered entry to make room), but the total count
        // should NOT exceed the cap.
        assert!(pool.discover(fake_xauth(MAX_UNVALIDATED), "test"));
        assert_eq!(pool.snapshot().len(), MAX_UNVALIDATED);
    }

    #[test]
    fn mark_validated_updates_status_and_identity() {
        let pool = TokenPool::new();
        let t = fake_xauth(0);
        pool.discover(t.clone(), "test");
        pool.mark_validated(&t, Some("1234".into()), Some("GAMERTAG".into()));
        let snap = pool.snapshot();
        assert_eq!(snap[0].status, TokenStatus::Validated);
        assert_eq!(snap[0].xuid.as_deref(), Some("1234"));
        assert_eq!(snap[0].gamertag.as_deref(), Some("GAMERTAG"));
        assert!(snap[0].last_validated_at > 0);
    }

    #[test]
    fn mark_validated_overwrites_identity_on_rebind() {
        // New semantics: if Xbox rotates the token's principal on
        // a refresh, we accept the new identity rather than keeping
        // the stale one. (Prior behavior: only set when None; that
        // masked the real account-switch case.)
        let pool = TokenPool::new();
        let t = fake_xauth(0);
        pool.discover(t.clone(), "test");
        pool.mark_validated(&t, Some("orig".into()), None);
        pool.mark_validated(&t, Some("new".into()), None);
        let snap = pool.snapshot();
        assert_eq!(snap[0].xuid.as_deref(), Some("new"));
    }

    #[test]
    fn mark_validated_caps_per_xuid_fallbacks() {
        let pool = TokenPool::new();
        // Four tokens, all for the same XUID.
        for i in 0..4 {
            let t = fake_xauth(i);
            pool.discover(t.clone(), "test");
            pool.mark_validated(&t, Some("sharedxuid".into()), None);
        }
        // Should collapse to MAX_PER_XUID (3).
        let snap = pool.snapshot();
        let same_xuid_count = snap
            .iter()
            .filter(|e| e.xuid.as_deref() == Some("sharedxuid"))
            .count();
        assert_eq!(same_xuid_count, MAX_PER_XUID);
    }

    #[test]
    fn mark_validated_keeps_just_validated_token_under_tie() {
        // BUG HUNTER pool-evict-determinism regression: MAX_PER_XUID+1
        // tokens for one XUID, all validated within the same second (a
        // tie on `last_validated_at`). The token validated LAST must
        // always survive the cap eviction — previously the tie-break fell
        // back to nondeterministic `HashMap` iteration order and could
        // evict the freshly-promoted token while keeping an older sibling.
        let pool = TokenPool::new();
        let mut tokens = Vec::new();
        for i in 0..=MAX_PER_XUID {
            let t = fake_xauth(i);
            pool.discover(t.clone(), "test");
            pool.mark_validated(&t, Some("sharedxuid".into()), None);
            tokens.push(t);
        }
        let last = tokens.last().expect("at least one token");
        let snap = pool.snapshot();
        // Cap honoured…
        let same_xuid_count = snap
            .iter()
            .filter(|e| e.xuid.as_deref() == Some("sharedxuid"))
            .count();
        assert_eq!(same_xuid_count, MAX_PER_XUID);
        // …and the LAST-validated token is the one retained.
        assert!(
            snap.iter().any(|e| &e.xauth == last),
            "just-validated token was evicted under a same-second tie"
        );
    }

    #[test]
    fn is_usable_rejects_unvalidated() {
        let e = TokenEntry {
            xauth: "x".into(),
            xuid: None,
            gamertag: None,
            status: TokenStatus::Discovered,
            discovered_at: 100,
            last_validated_at: 0,
            last_used_at: 0,
            last_401_at: 0,
            source: String::new(),
        };
        assert!(!e.is_usable(1000));
    }

    #[test]
    fn is_usable_accepts_fresh_validated() {
        let e = TokenEntry {
            xauth: "x".into(),
            xuid: Some("1".into()),
            gamertag: None,
            status: TokenStatus::Validated,
            discovered_at: 100,
            last_validated_at: 1000,
            last_used_at: 0,
            last_401_at: 0,
            source: String::new(),
        };
        assert!(e.is_usable(1000));
    }

    #[test]
    fn is_usable_rejects_stale_validated() {
        let e = TokenEntry {
            xauth: "x".into(),
            xuid: Some("1".into()),
            gamertag: None,
            status: TokenStatus::Validated,
            discovered_at: 100,
            last_validated_at: 0,
            last_used_at: 0,
            last_401_at: 0,
            source: String::new(),
        };
        // 15 min + 1 sec = definitely stale.
        assert!(!e.is_usable(VALIDATION_TTL.as_secs() as i64 + 1));
    }

    #[test]
    fn needs_revalidation_true_for_discovered() {
        let e = TokenEntry {
            xauth: "x".into(),
            xuid: None,
            gamertag: None,
            status: TokenStatus::Discovered,
            ..blank()
        };
        assert!(e.needs_revalidation(100));
    }

    #[test]
    fn needs_revalidation_false_for_quarantined() {
        let e = TokenEntry {
            xauth: "x".into(),
            xuid: None,
            gamertag: None,
            status: TokenStatus::Quarantined,
            last_401_at: 100,
            ..blank()
        };
        assert!(!e.needs_revalidation(200));
    }

    #[test]
    fn should_evict_only_after_quarantine_ttl() {
        let e = TokenEntry {
            xauth: "x".into(),
            xuid: None,
            gamertag: None,
            status: TokenStatus::Quarantined,
            last_401_at: 100,
            ..blank()
        };
        // Within TTL: don't evict.
        assert!(!e.should_evict(100 + QUARANTINE_TTL.as_secs() as i64 - 1));
        // Past TTL: evict.
        assert!(e.should_evict(100 + QUARANTINE_TTL.as_secs() as i64 + 1));
    }

    #[test]
    fn pick_for_request_returns_none_when_empty() {
        let pool = TokenPool::new();
        assert!(pool.pick_for_request().is_none());
    }

    #[test]
    fn pick_for_request_returns_none_when_no_validated() {
        let pool = TokenPool::new();
        pool.discover(fake_xauth(0), "test");
        // Only discovered, not validated → no usable.
        assert!(pool.pick_for_request().is_none());
    }

    #[test]
    fn pick_for_request_round_robins_across_distinct_xuids() {
        let pool = TokenPool::new();
        // 3 tokens, 3 DIFFERENT xuids — rotation should hit all 3.
        for i in 0..3 {
            let t = fake_xauth(i);
            pool.discover(t.clone(), "test");
            pool.mark_validated(&t, Some(format!("xuid{i}")), None);
        }
        let mut picks = Vec::new();
        for _ in 0..6 {
            picks.push(pool.pick_for_request().unwrap().xuid.unwrap());
        }
        let uniq: std::collections::HashSet<_> = picks.iter().collect();
        assert_eq!(uniq.len(), 3);
    }

    #[test]
    fn pick_for_request_collapses_same_xuid_to_one_bucket() {
        // 5 tokens, 1 xuid → picker must round-robin over 1 bucket
        // (no fake multiplication of budget). Every pick returns
        // SOMETHING usable, but the unique XUIDs seen = 1.
        let pool = TokenPool::new();
        for i in 0..5 {
            let t = fake_xauth(i);
            pool.discover(t.clone(), "test");
            pool.mark_validated(&t, Some("onexuid".into()), None);
        }
        let mut uniq_xuids = std::collections::HashSet::new();
        for _ in 0..10 {
            uniq_xuids.insert(pool.pick_for_request().unwrap().xuid.unwrap());
        }
        assert_eq!(uniq_xuids.len(), 1);
    }

    #[test]
    fn evict_dead_removes_expired_quarantine() {
        let pool = TokenPool::new();
        let t = fake_xauth(0);
        pool.discover(t.clone(), "test");
        pool.mark_quarantined(&t);
        // Force last_401_at into the past.
        {
            let mut map = pool.inner.tokens.write().unwrap();
            let e = map.get_mut(&t).unwrap();
            e.last_401_at = now_unix() - QUARANTINE_TTL.as_secs() as i64 - 10;
        }
        let evicted = pool.evict_dead();
        assert_eq!(evicted, 1);
        assert_eq!(pool.snapshot().len(), 0);
    }

    #[test]
    fn evict_dead_removes_expired_discovered() {
        // New behavior: Discovered entries aged past DISCOVERED_TTL
        // are memory-scan junk that never validated. Evict them so
        // the pool stays clean.
        let pool = TokenPool::new();
        let t = fake_xauth(0);
        pool.discover(t.clone(), "test");
        {
            let mut map = pool.inner.tokens.write().unwrap();
            let e = map.get_mut(&t).unwrap();
            e.discovered_at = now_unix() - DISCOVERED_TTL.as_secs() as i64 - 1;
        }
        assert_eq!(pool.evict_dead(), 1);
    }

    #[test]
    fn evict_dead_preserves_fresh_quarantine() {
        let pool = TokenPool::new();
        let t = fake_xauth(0);
        pool.discover(t.clone(), "test");
        pool.mark_quarantined(&t);
        assert_eq!(pool.evict_dead(), 0);
        assert_eq!(pool.snapshot().len(), 1);
    }

    #[test]
    fn prune_unvalidated_removes_only_discovered() {
        let pool = TokenPool::new();
        let t1 = fake_xauth(0);
        let t2 = fake_xauth(1);
        let t3 = fake_xauth(2);
        pool.discover(t1.clone(), "test");
        pool.discover(t2.clone(), "test");
        pool.discover(t3.clone(), "test");
        pool.mark_validated(&t1, Some("x1".into()), None);
        pool.mark_quarantined(&t2);
        // t3 stays Discovered.
        let dropped = pool.prune_unvalidated();
        assert_eq!(dropped, 1);
        assert_eq!(pool.snapshot().len(), 2);
    }

    #[test]
    fn masked_hides_token_body() {
        let e = TokenEntry {
            xauth: "XBL3.0 x=12345;verylongtokenbodyhere".into(),
            ..blank()
        };
        let masked = e.masked();
        assert!(masked.contains("12345"));
        assert!(!masked.contains("verylongtokenbodyhere"));
    }

    #[test]
    fn distinct_xuids_only_counts_validated() {
        let pool = TokenPool::new();
        let t1 = fake_xauth(0);
        let t2 = fake_xauth(1);
        pool.discover(t1.clone(), "test");
        pool.discover(t2, "test");
        pool.mark_validated(&t1, Some("xuid1".into()), None);
        // Second token stays "discovered" — shouldn't count.
        assert_eq!(pool.distinct_xuids(), vec!["xuid1"]);
    }

    fn blank() -> TokenEntry {
        TokenEntry {
            xauth: String::new(),
            xuid: None,
            gamertag: None,
            status: TokenStatus::Discovered,
            discovered_at: 0,
            last_validated_at: 0,
            last_used_at: 0,
            last_401_at: 0,
            source: String::new(),
        }
    }
}
