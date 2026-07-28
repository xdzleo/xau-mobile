//! Local library of saved plan JSON files.
//!
//! Stored under `%USERPROFILE%/Documents/XAU/plans/`. Each plan
//! is a file whose contents match the `Plan` type; the directory
//! is the user's personal collection of scans (self + followed
//! profiles + imported plans).
//!
//! Responsibilities here:
//! - Ensure the directory exists.
//! - Enumerate contents with lightweight metadata (parse only
//!   enough of each file to show it in the UI without loading the
//!   full entries vec for every row).
//! - Save a `Plan` in-memory to disk with a deterministic filename.
//! - Load a specific file back into memory.
//!
//! The aggregate feature (`plan::aggregate::best_of_aggregate`)
//! consumes file lists from here — the typical flow is: enumerate,
//! let the user pick a subset via checkboxes, load each, then
//! aggregate.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::plan::Plan;
use crate::settings;

/// Global serialization lock for `save_plan`. Without it, two parallel
/// background scans completing in the same millisecond BOTH see
/// "filename doesn't exist yet" during the collision check and then
/// overwrite each other's writes. The lock is coarse-grained (one
/// global Mutex for all save_plan calls) because plan serialization
/// is typically <10ms — contention is not a real issue. See audit
/// bug #32.
static SAVE_PLAN_LOCK: Mutex<()> = Mutex::new(());

/// Lightweight metadata parsed out of a plan file without loading
/// its `entries` vec. Small enough that listing a 50-file directory
/// stays under a few MB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFile {
    pub path: String,
    pub filename: String,
    pub source_gamertag: String,
    pub source_xuid: String,
    pub built_at: String,
    /// Entries count taken from the plan's `total` field (O(1) read,
    /// no need to parse the whole entries vec).
    pub total_entries: usize,
    /// Distinct `title_id`s — requires parsing the entries vec, so
    /// populated only when `deep` is true in `list_plans`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique_titles: Option<usize>,
    pub size_bytes: u64,
    pub modified_unix: i64,
    /// Why the scan's achievements didn't all become entries. Read
    /// straight off the file header — no need to touch the entries vec
    /// — so the library can explain a small plan without a deep parse.
    /// Zeroed for plans built before this was recorded.
    #[serde(default)]
    pub build_stats: crate::plan::PlanBuildStats,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PlanFileHeader {
    source_gamertag: String,
    source_xuid: String,
    built_at: String,
    total: usize,
    build_stats: crate::plan::PlanBuildStats,
}

/// `Documents/XAU/plans/`, created if missing.
pub fn plans_dir() -> Result<PathBuf> {
    let dir = settings::settings_dir()?.join("plans");
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

/// Enumerate every `*.json` file in the plans dir. Each entry gets
/// lightweight metadata. Malformed files (not parseable as `Plan`)
/// are skipped with a tracing warning, not bubbled up — the UI
/// should always render even if one file on disk is corrupted.
///
/// `deep = true` also reports `unique_titles` per file, which
/// requires loading the full entries vec. Use for small lists.
pub fn list_plans(deep: bool) -> Result<Vec<PlanFile>> {
    let dir = plans_dir()?;
    let mut out: Vec<PlanFile> = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(?e, path = %dir.display(), "plans_dir unreadable");
            return Ok(out);
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, %filename, "plan file read failed, skipping");
                continue;
            }
        };
        let parsed = if deep {
            match serde_json::from_str::<Plan>(&raw) {
                Ok(plan) => {
                    let mut set: std::collections::HashSet<&String> =
                        std::collections::HashSet::new();
                    for e in &plan.entries {
                        set.insert(&e.title_id);
                    }
                    (
                        plan.source_gamertag,
                        plan.source_xuid,
                        plan.built_at,
                        plan.total,
                        Some(set.len()),
                        plan.build_stats,
                    )
                }
                Err(e) => {
                    tracing::warn!(?e, %filename, "plan file parse failed, skipping");
                    continue;
                }
            }
        } else {
            match serde_json::from_str::<PlanFileHeader>(&raw) {
                Ok(header) => (
                    header.source_gamertag,
                    header.source_xuid,
                    header.built_at,
                    header.total,
                    None,
                    header.build_stats,
                ),
                Err(e) => {
                    tracing::warn!(?e, %filename, "plan file header parse failed, skipping");
                    continue;
                }
            }
        };
        let size_bytes = fs::metadata(&path).map_or(0, |m| m.len());
        let modified_unix = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let (source_gamertag, source_xuid, built_at, total_entries, unique_titles, build_stats) =
            parsed;
        out.push(PlanFile {
            path: path.display().to_string(),
            filename,
            source_gamertag,
            source_xuid,
            built_at,
            total_entries,
            build_stats,
            unique_titles,
            size_bytes,
            modified_unix,
        });
    }
    // Newest first — matches user expectation in file pickers.
    out.sort_by(|a, b| b.modified_unix.cmp(&a.modified_unix));
    Ok(out)
}

/// Load a single plan file. Caller provides the path (typically
/// from a prior `list_plans` result).
///
/// BUG HUNTER M-plan-validate-load: previously this only parsed
/// JSON — a hand-edited file with `{"title_id": "", "service_config_id": "",
/// "achievement_id": "x"}` would deserialize cleanly and reach the
/// runner, which tried to POST to `/title//achievements//update`
/// → 404 with no useful diagnostic. Now we filter entries that
/// have BOTH `title_id` and `service_config_id` empty (those are
/// impossible to dispatch), surface the count via tracing so an
/// operator can audit it, and reject the load entirely if EVERY
/// entry was invalid.
/// Hard cap on plan-file size when loading from disk. 50 MiB is
/// comfortably above any plausible real-user profile (the largest
/// historically observed is ~80 MB for a 240k-entry aggregate, but
/// that's still under the cap and we'd warn rather than reject).
/// Above 50 MiB we refuse to load — that's either a corrupted file,
/// a maliciously-crafted community plan, or a path that points at
/// something that isn't a plan at all (a video file that ended up
/// in the plans dir). Wave-10 audit resource-F4.
const PLAN_FILE_HARD_CAP_BYTES: u64 = 50 * 1024 * 1024;

pub fn load_plan(path: &Path) -> Result<Plan> {
    // Wave-10 audit resource-F4: check file size BEFORE
    // `read_to_string` (which buffers the whole file in memory).
    // A 4 GB JSON masquerading as a plan would otherwise pin
    // however much RAM the OS will give the process. The 50 MiB
    // cap is enough for the largest real aggregate plans plus
    // headroom; a legitimate "huge" plan should be split / paged.
    let metadata = fs::metadata(path)
        .map_err(|e| Error::other(format!("stat plan `{}`: {e}", path.display())))?;
    if metadata.len() > PLAN_FILE_HARD_CAP_BYTES {
        return Err(Error::other(format!(
            "plan file `{}` is {} bytes (cap {}); refusing to load. \
             Either the file is corrupt or this isn't a plan JSON.",
            path.display(),
            metadata.len(),
            PLAN_FILE_HARD_CAP_BYTES,
        )));
    }
    let raw = fs::read_to_string(path)
        .map_err(|e| Error::other(format!("reading plan `{}`: {e}", path.display())))?;
    let mut plan: Plan = serde_json::from_str::<Plan>(&raw)
        .map_err(|e| Error::other(format!("parsing plan `{}`: {e}", path.display())))?;

    let original_count = plan.entries.len();
    plan.entries.retain(|e| {
        // Reject entries the runner can never dispatch. Empty
        // achievement_id is also unusable — Xbox endpoint URL
        // requires it.
        !(e.title_id.trim().is_empty() && e.service_config_id.trim().is_empty())
            && !e.achievement_id.trim().is_empty()
    });
    let dropped = original_count - plan.entries.len();
    if dropped > 0 {
        tracing::warn!(
            path = %path.display(),
            dropped,
            kept = plan.entries.len(),
            "load_plan: dropped invalid entries (empty title+scid or empty ach_id)",
        );
    }
    if plan.entries.is_empty() && original_count > 0 {
        return Err(Error::other(format!(
            "plan `{}` had {original_count} entries but ALL were invalid (empty title+scid or empty ach_id) — file is corrupted",
            path.display()
        )));
    }
    // Refresh `total` to match the cleaned entries vector — the
    // pre-existing total field could be stale if the file was
    // partially corrupted.
    plan.total = plan.entries.len();
    Ok(plan)
}

/// Sanitize a string for use as a filename component: alphanumeric,
/// dash, underscore only; trim to 40 chars. Prevents arbitrary
/// characters from the Xbox gamertag (spaces, apostrophes, emoji,
/// etc.) from breaking `fs::write`.
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.chars().take(40).collect::<String>()
}

/// Generate a deterministic filename: `{gamertag}-{xuid}-{yyyymmdd}.json`.
/// If the file already exists, appends `-v2`, `-v3`, etc. so an
/// auto-save doesn't silently overwrite yesterday's snapshot of
/// the same profile.
pub fn make_default_filename(plan: &Plan) -> String {
    let gt = sanitize(&plan.source_gamertag);
    let xuid = sanitize(&plan.source_xuid);
    // CHAR-wise truncation, NOT byte-wise. Previous code used
    // `plan.built_at[..len.min(10)]` which panics with "not on char
    // boundary" when built_at begins with a multibyte UTF-8 character
    // (e.g. from a corrupted plan JSON). Input comes from disk so we
    // can't assume ASCII. See audit bugs #13 / #19.
    let date: String = plan.built_at.chars().take(10).collect();
    let date_compact: String = date.chars().filter(char::is_ascii_digit).collect();
    // Empty date still produces a usable filename (date suffix just
    // absent). Collision handling in save_plan() disambiguates via -v2.
    let date_part = if date_compact.is_empty() {
        "undated".to_string()
    } else {
        date_compact
    };
    if gt.is_empty() {
        format!("plan-{xuid}-{date_part}.json")
    } else {
        format!("{gt}-{xuid}-{date_part}.json")
    }
}

fn safe_filename_override(filename: &str) -> Result<String> {
    let trimmed = filename.trim();
    if trimmed.is_empty() {
        return Err(Error::invalid_argument("filename override is empty"));
    }
    let path = Path::new(trimmed);
    if path.is_absolute()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains(':')
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Error::invalid_argument(
            "filename override must be a plain file name inside the plans directory",
        ));
    }
    Ok(trimmed.to_string())
}

/// Save a plan to the plans dir. If `filename` is `None`, uses
/// `make_default_filename`. If the chosen filename already exists,
/// auto-suffixes `-v2`, `-v3`, etc. Returns the absolute path
/// written.
pub fn save_plan(plan: &Plan, filename: Option<&str>) -> Result<PathBuf> {
    let dir = plans_dir()?;
    let base = match filename {
        Some(name) => safe_filename_override(name)?,
        None => make_default_filename(plan),
    };
    // Split base into stem + ext so suffix insertion stays clean.
    let (stem, ext) = match base.rfind('.') {
        Some(idx) if idx > 0 => (&base[..idx], &base[idx..]),
        _ => (base.as_str(), ".json"),
    };
    // Serialize the collision-check + write across all callers so
    // two concurrent saves for the same target can't both observe
    // "filename doesn't exist yet" and then overwrite each other.
    // See audit bug #32.
    let _guard = SAVE_PLAN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // BUG HUNTER M-toctou-saveplan: previously this was
    // `while candidate.exists() { ... } fs::write(candidate, json)`
    // — a classic TOCTOU. Two concurrent xau processes (xau-cli +
    // xau-gui both running, both pointing at the same plans dir)
    // race past `exists()` and the second `fs::write` clobbers the
    // first. The in-process `SAVE_PLAN_LOCK` doesn't help between
    // separate processes. Fix: open with `create_new` so the OS
    // does the atomic existence check + create. On collision,
    // bump the version suffix and try again. `create_new` is
    // honoured on both Windows (NTFS) and Linux (ext4).
    let json = serde_json::to_string_pretty(plan)
        .map_err(|e| Error::other(format!("serialize plan: {e}")))?;
    use std::io::Write;
    let mut candidate = dir.join(&base);
    let mut version: u32 = 2;
    // Phase 1 — Reserve the final filename across processes by
    // atomically creating a 0-byte sentinel via `create_new`. If two
    // xau processes try the same plan name concurrently, only one
    // will win that exact path; the other bumps to `-v2`, etc.
    let sentinel = loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => break f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let try_name = format!("{stem}-v{version}{ext}");
                candidate = dir.join(&try_name);
                version += 1;
                if version > 999 {
                    return Err(Error::other(format!(
                        "could not find a non-colliding filename for `{base}` in plans dir"
                    )));
                }
            }
            Err(e) => {
                return Err(Error::other(format!(
                    "creating plan file `{}`: {e}",
                    candidate.display()
                )));
            }
        }
    };
    // Drop the sentinel handle — we own the path now; the 0-byte
    // placeholder will be replaced atomically by the rename below.
    drop(sentinel);

    // Phase 2 — Torn-write protection (BUG HUNTER M-tornwrite-saveplan):
    // write the full payload to a sibling `*.tmp.<pid>` file, fsync
    // it for durability, then atomic-rename over the sentinel.
    // `std::fs::rename` is atomic-and-replaces on both POSIX
    // (rename(2)) and Windows (MoveFileExW with MOVEFILE_REPLACE_EXISTING),
    // so a power loss either leaves the OLD content (the 0-byte
    // sentinel — recoverable, will be bumped to -v2 on next save)
    // or the FULL new content. Never a half-written file.
    let pid = std::process::id();
    let tmp_path = candidate.with_file_name(format!(
        "{}.tmp.{pid}",
        candidate
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("plan.json"),
    ));
    let write_result = (|| -> Result<()> {
        let mut tmp = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| Error::other(format!("creating temp `{}`: {e}", tmp_path.display())))?;
        tmp.write_all(json.as_bytes())
            .map_err(|e| Error::other(format!("writing temp `{}`: {e}", tmp_path.display())))?;
        // Durability: fsync before the rename so the OS can't
        // reorder data writes after the metadata commit. Cheap on
        // SSD (~1-3ms) and tiny vs the cost of rebuilding a plan.
        tmp.sync_all()
            .map_err(|e| Error::other(format!("fsync temp `{}`: {e}", tmp_path.display())))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        // Clean up both the half-written tmp and the sentinel so we
        // don't leak a zero-byte file that will block the next save.
        let _ = fs::remove_file(&tmp_path);
        let _ = fs::remove_file(&candidate);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp_path, &candidate) {
        let _ = fs::remove_file(&tmp_path);
        let _ = fs::remove_file(&candidate);
        return Err(Error::other(format!(
            "atomic rename `{}` → `{}`: {e}",
            tmp_path.display(),
            candidate.display(),
        )));
    }
    tracing::info!(
        path = %candidate.display(),
        entries = plan.entries.len(),
        "plan saved"
    );
    Ok(candidate)
}

/// Remove a plan file. Bounded to files inside `plans_dir` for
/// safety — the IPC layer passes absolute paths from a prior
/// `list_plans` result, but we double-check here so a malicious
/// caller can't delete arbitrary files by crafting a `path`.
pub fn delete_plan(path: &Path) -> Result<()> {
    let canonical_target = ensure_inside_plans_dir(path)?;
    fs::remove_file(&canonical_target)
        .map_err(|e| Error::other(format!("removing `{}`: {e}", canonical_target.display())))?;
    Ok(())
}

/// Validate that `path` resolves inside `plans_dir()`. Returns the
/// canonicalised target on success, or `InvalidArgument` if the
/// path escapes (or even *might* escape).
///
/// Hardened against the canonicalize-fallback escape: if `canonicalize`
/// fails (target doesn't exist, broken symlink, intermediate component
/// missing) we now reject the operation outright instead of falling
/// back to the raw `..`-laden input. The previous behaviour compared
/// the raw path with the canonical plans_dir prefix, which let
/// `Documents/XAU/plans/../../something.json` slip through whenever
/// canonicalize bailed early.
///
/// Public so the IPC layer (`plan.save` / `plan.load`) can re-use the
/// same gate that `delete_plan` uses.
pub fn ensure_inside_plans_dir(path: &Path) -> Result<PathBuf> {
    let dir = plans_dir()?;
    let canonical_dir = fs::canonicalize(&dir)
        .map_err(|e| Error::other(format!("canonicalize plans dir `{}`: {e}", dir.display())))?;
    // Reject paths with parent-dir / prefix components up-front — even
    // a successful canonicalize on a relative `..` path would resolve
    // to whatever the *current* CWD makes it, not what we want.
    for c in path.components() {
        if matches!(c, Component::ParentDir) {
            return Err(Error::invalid_argument(format!(
                "refuse `{}`: path contains `..` component",
                path.display()
            )));
        }
    }
    // For *new* writes (save) the file may not exist yet — canonicalize
    // the *parent* if direct canonicalize fails.
    let canonical_target = match fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            // Build best-effort canonical: canonicalize the parent + push file_name.
            let parent = path.parent().ok_or_else(|| {
                Error::invalid_argument(format!("refuse `{}`: no parent directory", path.display()))
            })?;
            let file_name = path.file_name().ok_or_else(|| {
                Error::invalid_argument(format!(
                    "refuse `{}`: no file name component",
                    path.display()
                ))
            })?;
            let canonical_parent = fs::canonicalize(parent).map_err(|e| {
                Error::invalid_argument(format!(
                    "refuse `{}`: parent `{}` not canonicalizable: {e}",
                    path.display(),
                    parent.display(),
                ))
            })?;
            canonical_parent.join(file_name)
        }
    };
    if !canonical_target.starts_with(&canonical_dir) {
        return Err(Error::invalid_argument(format!(
            "refuse `{}`: outside plans directory `{}`",
            path.display(),
            canonical_dir.display(),
        )));
    }
    Ok(canonical_target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_illegal_chars() {
        assert_eq!(sanitize("FixtureUser001"), "FixtureUser001");
        assert_eq!(sanitize("Player One 42"), "Player_One_42");
        assert_eq!(sanitize("ñ@me?"), "__me_");
    }

    #[test]
    fn make_default_filename_basic() {
        let p = Plan {
            source_gamertag: "FixtureUser001".into(),
            source_xuid: "2500000000000001".into(),
            built_at: "2026-04-23T12:34:56Z".into(),
            ..Default::default()
        };
        assert_eq!(
            make_default_filename(&p),
            "FixtureUser001-2500000000000001-20260423.json"
        );
    }

    #[test]
    fn make_default_filename_empty_gamertag() {
        let p = Plan {
            source_gamertag: String::new(),
            source_xuid: "123".into(),
            built_at: "2026-04-23T00:00:00Z".into(),
            ..Default::default()
        };
        assert_eq!(make_default_filename(&p), "plan-123-20260423.json");
    }

    #[test]
    fn filename_override_rejects_paths() {
        assert!(safe_filename_override("safe-name.json").is_ok());
        assert!(safe_filename_override("../evil.json").is_err());
        assert!(safe_filename_override("nested/evil.json").is_err());
        assert!(safe_filename_override(r"..\evil.json").is_err());
        assert!(safe_filename_override(r"C:\tmp\evil.json").is_err());
        assert!(safe_filename_override("").is_err());
    }
}
