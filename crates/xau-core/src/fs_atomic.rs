//! Atomic file-write helper.
//!
//! Wave-10/11 audit cross-cutting #4: multiple sites in the workspace
//! were doing direct `fs::write(path, data)`, which on a power loss /
//! kill -9 between the syscall and the FS metadata commit leaves a
//! half-written file. The downstream reader either crashes the parser
//! (pre-soft-fail) or silently loads a corrupted state. Two existing
//! sites already had bespoke temp+rename loops:
//!
//!   - [`crate::oauth::session::AuthSession::save_to`] (uses
//!     `<path>.tmp.<pid>` + a single rename — crash-unique, no retry)
//!   - [`crate::oauth::token_pool::TokenPool::save_to_disk`] (uses
//!     `<path>.json.tmp` + 6× rename retry + temp cleanup, for
//!     Windows AV resilience on the hot-path token file)
//!
//! `write_atomic` is the simpler `settings.rs` flavour — the right
//! choice for files that are NOT in the hot anti-virus path (plan
//! files, downloaded updaters, community caches, settings). The
//! `token_pool` retry-loop pattern is preserved separately; do NOT
//! migrate that one to this helper without porting the retries.
//!
//! ## Atomicity contract
//!
//! - POSIX `rename(2)` is atomic — power loss leaves either the OLD
//!   file or the FULL new content, never half-written.
//! - Windows `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` is also
//!   atomic on NTFS. `std::fs::rename` uses this internally.
//! - The `<path>.tmp.<pid>` naming makes recovery from a stranded
//!   tmp file trivial: any `*.tmp.<pid>` left behind by a previous
//!   crash can be safely deleted (the pid is no longer running).
//!
//! ## Cleanup-on-error
//!
//! Both `fs::write` and `fs::rename` can fail. On error we attempt
//! to remove the tmp file so a permission-denied or disk-full
//! failure doesn't strand a partial file in the directory.

use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;

/// Write `data` to `path` atomically: write to a sibling
/// `<filename>.tmp.<pid>` first, then rename over the destination.
///
/// On any error, attempts to remove the tmp file before returning.
/// Creates the parent directory if it doesn't exist (mirrors the
/// `settings.rs::save_to` contract).
///
/// Wave-10/11 audit: shared helper extracted to align the 5
/// torn-write sites we found. Replaces direct `fs::write(path,
/// data)` calls.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("xau.tmp");
    let tmp = path.with_file_name(format!("{file_name}.tmp.{pid}"));
    // BUG HUNTER fs-atomic-fsync: the module's durability contract
    // ("power loss leaves either the OLD file or the FULL new content")
    // was NOT actually upheld by the previous `fs::write` + `fs::rename`.
    // `rename` is atomic w.r.t. the *name*, but without flushing the tmp
    // file's DATA blocks first, a crash/power-loss can make the rename's
    // metadata durable while the tmp's contents are not — leaving the
    // destination pointing at zero-length or torn data after reboot,
    // exactly the corruption the helper claims to prevent (and the
    // updater relies on for the downloaded executable). Fix: `write_all`
    // then `sync_all()` on the tmp handle BEFORE the rename so the bytes
    // are on stable storage first. (Parent-directory fsync — which would
    // also make the rename itself crash-durable — is omitted: std has no
    // portable way to fsync a directory on the Windows release target.)
    let write_res = (|| -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn write_atomic_creates_file_with_correct_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hello.txt");
        write_atomic(&path, b"hello world").expect("write_atomic ok");
        let mut buf = Vec::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        assert_eq!(buf, b"hello world");
    }

    #[test]
    fn write_atomic_creates_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a/b/c/nested.json");
        write_atomic(&path, b"{}").expect("write_atomic with new parents");
        assert!(path.exists());
    }

    #[test]
    fn write_atomic_overwrites_existing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("over.txt");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, b"new content").expect("write_atomic over old");
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "new content");
    }

    #[test]
    fn write_atomic_does_not_strand_tmp_files() {
        // After a successful write, no `*.tmp.*` siblings should
        // remain in the directory. The rename moves the tmp into
        // the final path; nothing is left behind.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clean.json");
        write_atomic(&path, b"data").expect("ok");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "expected no .tmp. siblings, found: {leftovers:?}"
        );
    }
}
